use crate::process::ProcessJob;
use crate::{
    model::{
        AddRequest, AudioSelection, DownloadKind, ExternalSubtitle, InspectRequest, MediaFormat,
        MediaInfo, ScopedCookie, SubtitleSelection, SubtitleTrack, TransferControl,
        TransferProgress, TransferResult,
    },
    network::{ExternalProxy, NetworkGovernor},
    paths::AppPaths,
    tools,
};
use anyhow::{anyhow, bail, Context, Result};
use curl::easy::List;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use url::Url;
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

mod evidence;
mod metadata;
mod native_hls;
mod postprocess;
mod probe;
mod process;
mod selection;
mod session;
mod workspace;
use evidence::*;
use metadata::*;
pub(crate) use postprocess::validate_options;
use postprocess::*;
use probe::*;
use process::*;
pub(crate) use selection::selected_formats;
use selection::*;
use session::*;
pub(crate) use session::{scoped_cookie_applies, sweep_stale_cookie_jars};
use workspace::*;
pub(crate) use workspace::{
    cancel_source_refresh, finish_source_refresh, has_partial_output, prepare_source_refresh,
    retained_bytes, stage_source_refresh, PreparedSourceRefresh,
};

const INSPECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_INSPECT_OUTPUT: usize = 128 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 32 * 1024;
/// A downloaded stream must show progress within this window or its transfer is reconnected.
const STALL_LIMIT: Duration = Duration::from_secs(180);
/// A post-processor (merge, remux, audio conversion of a long file) is silent while FFmpeg works.
const POSTPROCESS_STALL_LIMIT: Duration = Duration::from_secs(60 * 60);
/// How often a stream may be re-downloaded after a skipped fragment or a stall.
const FRAGMENT_RESTARTS: u32 = 2;
const STALL_RESTARTS: u32 = 3;

const PROGRESS_PREFIX: &str = "SSDOWNLOAD_PROGRESS\t";
const POSTPROCESS_PREFIX: &str = "SSDOWNLOAD_POSTPROCESS\t";
const FILE_PREFIX: &str = "SSDOWNLOAD_FILE\t";

/// yt-dlp's wording for a tunnel the application's own loopback proxy refused.
fn local_tunnel_refused(stderr: &str) -> bool {
    stderr.contains("Unable to connect to proxy") || stderr.contains("Tunnel connection failed")
}

/// Inspects a web page or direct manifest using the verified application-private yt-dlp.
pub(crate) fn inspect_controlled(
    paths: &AppPaths,
    request: &InspectRequest,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<MediaInfo> {
    validate_web_url(&request.url)?;
    if control.stop_requested() {
        bail!(crate::i18n::ui(
            "Kaynak hazırlığı durduruldu",
            "Source preparation was stopped"
        ));
    }
    let toolchain = tools::verified_toolchain(paths, control)?;
    inspect_with_tools(request, &toolchain, control, network)
}

fn inspect_with_tools(
    request: &InspectRequest,
    toolchain: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<MediaInfo> {
    let (value, info) = inspect_document_with_tools(request, toolchain, control, network)?;
    remember_inspection(request, &value);
    Ok(info)
}

fn inspect_document_with_tools(
    request: &InspectRequest,
    toolchain: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<(Value, MediaInfo)> {
    let yt_dlp = &toolchain.yt_dlp;
    let deno = &toolchain.deno;
    let ffmpeg = &toolchain.ffmpeg;
    let mut secrets = SensitiveInputs::create(
        &request.url,
        &request.headers,
        request.referer.as_deref(),
        request.page_url.as_deref(),
        &request.session_cookies,
    )?;
    let args = common_args(deno, ffmpeg, request.playlist);
    let mut command_args = args;
    command_args.extend([
        "--dump-single-json".into(),
        "--skip-download".into(),
        "--no-allow-unplayable-formats".into(),
    ]);
    if request.live_from_start {
        command_args.push("--live-from-start".into());
    }
    // Mirrors the download-phase compatibility retry: generic:impersonate
    // covers only metadata of the generic extractor, so a bot-protected
    // origin that rejects the first attempt gets exactly one retry with the
    // tool's default impersonation client.
    let mut captured = run_ytdlp_capture(
        yt_dlp,
        &command_args,
        &mut secrets.config,
        &mut secrets.redactions,
        INSPECT_TIMEOUT,
        control,
        network,
    )?;
    // A refused local tunnel is capacity, not the source: wait briefly and ask once more.
    if !captured.status.success() && local_tunnel_refused(&captured.stderr) {
        for _ in 0..40 {
            if control.stop_requested() {
                bail!(crate::i18n::ui(
                    "Kaynak hazırlığı durduruldu",
                    "Source preparation was stopped"
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
        captured = run_ytdlp_capture(
            yt_dlp,
            &command_args,
            &mut secrets.config,
            &mut secrets.redactions,
            INSPECT_TIMEOUT,
            control,
            network,
        )?;
    }
    if !captured.status.success() && is_forbidden(&captured.stderr) {
        let mut retried_args = command_args.clone();
        retried_args.extend(["--impersonate".into(), "".into()]);
        captured = run_ytdlp_capture(
            yt_dlp,
            &retried_args,
            &mut secrets.config,
            &mut secrets.redactions,
            INSPECT_TIMEOUT,
            control,
            network,
        )?;
    }
    if !captured.status.success() {
        let detail = useful_error(&captured.stderr, &secrets.redactions);
        // The extractor text stays verbatim; the code tells the user (and support) which
        // stage failed without hiding the original diagnostic.
        if drm_protected(&captured.stderr) {
            crate::logging::record(crate::logging::Event::failure(
                "inspect.drm",
                crate::error_codes::MED_003,
                detail,
            ));
            crate::bail_code!(crate::error_codes::MED_003);
        }
        if is_forbidden(&detail) {
            crate::logging::record(crate::logging::Event::failure(
                "inspect.blocked",
                crate::error_codes::MED_007,
                detail.clone(),
            ));
            crate::bail_code!(crate::error_codes::MED_007, "{detail}");
        }
        if site_access_blocked(&captured.stderr) {
            let short = useful_error(&final_error_text(&captured.stderr), &secrets.redactions);
            crate::logging::record(crate::logging::Event::failure(
                "inspect.access",
                crate::error_codes::MED_008,
                detail,
            ));
            crate::bail_code!(crate::error_codes::MED_008, "{short}");
        }
        crate::logging::record(crate::logging::Event::failure(
            "inspect.failed",
            crate::error_codes::MED_006,
            detail.clone(),
        ));
        crate::bail_code!(
            crate::error_codes::MED_006,
            "{}",
            crate::i18n::ui_format!(
                "Medya analizi başarısız: {detail}",
                "Media analysis failed: {detail}",
                detail = detail
            )
        );
    }
    if captured.stdout.len() > MAX_INSPECT_OUTPUT {
        crate::bail_code!(
            crate::error_codes::MED_006,
            "{}",
            crate::i18n::ui_format!(
                "Medya analizi çıktısı {0} MB sınırını aştı",
                "The media analysis output exceeded the {0} MB limit",
                MAX_INSPECT_OUTPUT / (1024 * 1024)
            )
        );
    }
    let value: Value = serde_json::from_slice(&captured.stdout).context(crate::i18n::ui(
        "yt-dlp geçerli JSON medya bilgisi döndürmedi",
        "yt-dlp did not return valid JSON media information",
    ))?;
    let info = media_info_from_json(&value)?;
    validate_single_selection(&info, request.playlist)?;
    Ok((value, info))
}

/// How long an analysis answers the download of the same source: long enough to pick a
/// quality and press Download, short enough for the signed addresses inside it to hold.
const INSPECTION_REUSE: Duration = Duration::from_secs(120);

/// The picker's latest analysis, keyed by everything that shapes it.
static RECENT_INSPECTION: Mutex<Option<(String, Instant, Value)>> = Mutex::new(None);

fn inspection_key(request: &InspectRequest) -> String {
    use sha2::{Digest, Sha256};
    let identity = serde_json::to_vec(&(
        &request.url,
        &request.headers,
        &request.session_cookies,
        &request.referer,
        &request.page_url,
        request.playlist,
        request.live_from_start,
    ))
    .unwrap_or_default();
    hex::encode(Sha256::digest(identity))
}

fn remember_inspection(request: &InspectRequest, value: &Value) {
    *RECENT_INSPECTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some((inspection_key(request), Instant::now(), value.clone()));
}

/// The analysis the picker just made for this exact source, taken once: a download started
/// from the picker does not run yt-dlp a second time, and a retry analyzes afresh.
fn reuse_inspection(request: &InspectRequest) -> Option<(Value, MediaInfo)> {
    let mut recent = RECENT_INSPECTION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let matches = recent.as_ref().is_some_and(|(key, at, _)| {
        at.elapsed() < INSPECTION_REUSE && *key == inspection_key(request)
    });
    if !matches {
        return None;
    }
    let (_, _, value) = recent.take()?;
    let info = media_info_from_json(&value).ok()?;
    validate_single_selection(&info, request.playlist).ok()?;
    Some((value, info))
}

fn validate_single_selection(info: &MediaInfo, playlist: bool) -> Result<()> {
    if !playlist && info.playlist_count.is_some_and(|count| count > 1) {
        crate::bail_code!(
            crate::error_codes::MED_010,
            "{}",
            crate::i18n::ui(
                "Sayfa birden fazla bağımsız video içeriyor; fragman veya reklam seçilmemesi için videoyu tarayıcıda oynatıp video üzerindeki düğmeyi kullanın",
                "The page contains more than one independent video; to avoid picking a trailer or ad, play the video in the browser and use the button on the video"
            )
        );
    }
    Ok(())
}

/// Downloads and post-processes media through an owned yt-dlp/FFmpeg process tree.
pub(crate) fn download(
    paths: &AppPaths,
    request: &AddRequest,
    output: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
    job_id: &str,
) -> Result<TransferResult> {
    validate_web_url(&request.url)?;
    reject_unsafe_media_headers(&request.headers)?;
    if control.is_cancelled() {
        crate::bail_code!(crate::error_codes::TRF_008);
    }
    if control.is_paused() {
        crate::bail_code!(crate::error_codes::TRF_009);
    }
    let from_start;
    let request = if request.live_from_start {
        from_start = live_from_start_request(request);
        &from_start
    } else {
        request
    };

    let effective_referer = request.referer.as_deref();
    let toolchain = tools::verified_toolchain(paths, control)?;
    let (mut info, source_capture, selector, failure_stages, from_start) =
        if let Some((pinned, evidence)) = load_pinned_source_for_request(output, request)? {
            let info = media_info_from_json(&pinned.inspection)?;
            let from_start = from_start_formats(&pinned.inspection);
            let selector = format_selector(request, &info)?;
            let failure_stages =
                DownloadFailureStages::from_inspection(&pinned.inspection, request, &selector);
            (
                info,
                MediaSourceCapture {
                    evidence: Some(evidence),
                    pinned: Some(pinned),
                    fallback_inspection: None,
                },
                selector,
                failure_stages,
                from_start,
            )
        } else {
            let mut inspect_request = InspectRequest {
                url: request.url.clone(),
                headers: request.headers.clone(),
                referer: effective_referer.map(str::to_string),
                page_url: request.page_url.clone(),
                session_cookies: request.session_cookies.clone(),
                playlist: request.playlist,
                live_from_start: request.live_from_start,
                ..InspectRequest::default()
            };
            let mut inspected = reuse_inspection(&inspect_request).map_or_else(
                || inspect_document_with_tools(&inspect_request, &toolchain, control, network),
                Ok,
            );
            // Which sites keep a running stream from its beginning is yt-dlp's knowledge; where
            // it finds none, the stream is recorded from now on instead of failing.
            if inspected.is_err() && inspect_request.live_from_start && !control.stop_requested() {
                inspect_request.live_from_start = false;
                inspected =
                    inspect_document_with_tools(&inspect_request, &toolchain, control, network);
            }
            let (inspection, info) = inspected.map_err(|error| {
                let phase = if media_manifest_url(&request.url) {
                    "ana liste"
                } else {
                    "keşif"
                };
                anyhow!(
                    "{}",
                    crate::i18n::ui_format!(
                        "{0} aşamasında {error:#}",
                        "{0} stage: {error:#}",
                        failure_stage_label(phase),
                        error = error
                    )
                )
            })?;
            let from_start = from_start_formats(&inspection);
            let selector = format_selector(request, &info)?;
            let failure_stages =
                DownloadFailureStages::from_inspection(&inspection, request, &selector);
            let capture =
                capture_media_source(request, &inspection, &info, &selector, control, network)?;
            (info, capture, selector, failure_stages, from_start)
        };
    if info.duration_tolerance.is_none() && !request.playlist {
        info.duration_tolerance = source_capture
            .pinned
            .as_ref()
            .and_then(pinned_hls_duration_tolerance);
    }
    if let Some(pinned) = source_capture.pinned.as_ref().filter(|_| !request.playlist) {
        (info.declared_video_durations, info.declared_audio_durations) =
            pinned_hls_declared_durations(pinned);
    }
    validate_track_selection(request, &info)?;
    validate_container_selection(request, &info)?;
    let live = info.is_live;
    if live {
        control.mark_live();
        crate::logging::record_job(
            job_id,
            crate::logging::Event::warn("download.live")
                .detail(format!("{} canlı yayın kaydı", crate::error_codes::MED_009)),
        );
    }
    // A live source has no end the app can predict: say so on every progress update instead of
    // the plain "Downloading media" that would read as a stuck download.
    let mut report = |mut update: TransferProgress| {
        if live && update.phase == crate::i18n::ui("Medya indiriliyor", "Downloading media") {
            update.phase = if request.live_from_start && !from_start {
                crate::i18n::ui(
                    "Site yayının başını sunmuyor; canlı yayın şu andan itibaren kaydediliyor",
                    "The site does not offer the stream's beginning; recording it from now on",
                )
            } else {
                crate::i18n::ui(
                    "Canlı yayın kaydediliyor; yayın bitene kadar sürer",
                    "Recording a live stream; it runs until the stream ends",
                )
            }
            .into();
        }
        progress(update);
    };
    let progress: &mut dyn FnMut(TransferProgress) = &mut report;
    let yt_dlp = &toolchain.yt_dlp;
    let deno = &toolchain.deno;
    let ffmpeg = &toolchain.ffmpeg;

    let conversion = audio_conversion(request)?;
    let output_plan = OutputPlan::new(output, request, source_capture.evidence.as_ref())?;
    if let Some(parent) = output_plan.template.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(pinned) = source_capture.pinned.as_ref() {
        output_plan.install_pinned_source(pinned)?;
    }
    let source_info = if source_capture.pinned.is_some() {
        Some(SourceInfoLease::from_pinned_workspace(
            &output_plan.workspace,
        )?)
    } else if let Some(inspection) = source_capture.fallback_inspection.as_ref() {
        Some(SourceInfoLease::from_inspection(inspection)?)
    } else {
        None
    };

    let mut secrets = SensitiveInputs::create_for_download(
        &request.url,
        &request.headers,
        effective_referer,
        request.page_url.as_deref(),
        &request.session_cookies,
        source_info.is_none(),
    )?;
    let mut base_args = common_args(deno, ffmpeg, request.playlist);
    if let Some(items) = request
        .playlist_items
        .as_deref()
        .filter(|_| request.playlist)
    {
        base_args.extend(["--playlist-items".into(), items.to_owned()]);
    }
    // Retain the tool's request trace only in bounded, redacted failure output so
    // an HTTP status can be tied to the selected media graph node.
    base_args.push("--verbose".into());
    if from_start {
        base_args.push("--live-from-start".into());
    }
    if live {
        // FFmpeg records a live DASH stream into a plain MP4, whose index is written only when
        // FFmpeg ends on its own, and finishing a recording stops it by force: fragments keep
        // what was recorded readable. Keyframes cut fragments only in a video track, so a
        // recording without video is cut every 5 s instead. Live HLS is recorded as MPEG-TS,
        // which ignores these options.
        base_args.extend([
            "--downloader-args".into(),
            "ffmpeg_o:-movflags +frag_keyframe+empty_moov -frag_duration 5000000".into(),
        ]);
    }
    if let Some(source_info) = &source_info {
        base_args.extend(load_info_args(
            &source_info.info_path,
            source_capture.pinned.is_some(),
        ));
    }
    base_args.extend([
        "--continue".into(),
        "--part".into(),
        "--no-overwrites".into(),
        "--no-keep-video".into(),
        // 20 fragment retries plus --abort-on-unavailable-fragments: a transient
        // tunnel/503 error must not kill a multi-GB VOD, but a stream must never be
        // completed with a missing fragment. Sleeps are capped at 15s so a
        // permanently unavailable fragment surfaces within minutes while the
        // backoff still bounds retry pressure toward signed origins.
        "--fragment-retries".into(),
        "20".into(),
        // Never accept a hole: without this yt-dlp writes the stream with the missing fragment.
        "--abort-on-unavailable-fragments".into(),
        "--retry-sleep".into(),
        "fragment:exp=1:15".into(),
        "--retry-sleep".into(),
        "http:exp=1:15".into(),
        "--retries".into(),
        "3".into(),
        "--file-access-retries".into(),
        "3".into(),
        "--windows-filenames".into(),
        "--no-allow-unplayable-formats".into(),
        "--newline".into(),
        "--progress".into(),
        "--progress-template".into(),
        format!("download:{PROGRESS_PREFIX}%(progress.status)s\t%(progress.downloaded_bytes)s\t%(progress.total_bytes,progress.total_bytes_estimate)s\t%(progress.speed)s\t%(progress.eta)s"),
        "--progress-template".into(),
        format!("postprocess:{POSTPROCESS_PREFIX}%(progress.status)s"),
        "--print".into(),
        format!("after_move:{FILE_PREFIX}%(filepath)j"),
        "--format".into(), selector,
        "--output".into(), output_plan.template.to_string_lossy().into_owned(),
    ]);

    // Fragment connections beyond the governor's per-host bound only churn
    // proxy 503s without adding throughput: the effective parallelism was
    // always min(requested, per_host_limit). Ask for exactly that so the
    // child never fights its own proxy for tunnel slots.
    // Fragment parallelism is appended per attempt: each retry after a lost fragment halves it.
    if request.kind != DownloadKind::Audio {
        if let Some(container) = request.container.as_deref() {
            base_args.extend([
                "--merge-output-format".into(),
                container.into(),
                "--remux-video".into(),
                container.into(),
            ]);
        }
    }
    if !request.audio_tracks.is_empty() {
        base_args.push("--audio-multistreams".into());
    }
    if request.kind == DownloadKind::Audio {
        base_args.extend([
            "--extract-audio".into(),
            "--audio-format".into(),
            conversion.unwrap_or("best").into(),
        ]);
        // yt-dlp's default MP3 quality is VBR 5 (about 130 kbps); the best source audio is kept
        // at V0. Other conversions take no VBR scale.
        if conversion == Some("mp3") {
            base_args.extend(["--audio-quality".into(), "0".into()]);
        }
    }
    if !request.subtitle_tracks.is_empty() {
        let manual = request.subtitle_tracks.iter().any(|track| !track.automatic);
        let automatic = request.subtitle_tracks.iter().any(|track| track.automatic);
        base_args.push(
            if manual {
                "--write-subs"
            } else {
                "--no-write-subs"
            }
            .into(),
        );
        base_args.push(
            if automatic {
                "--write-auto-subs"
            } else {
                "--no-write-auto-subs"
            }
            .into(),
        );
        let languages = request
            .subtitle_tracks
            .iter()
            .map(|track| track.language.as_str())
            .collect::<Vec<_>>()
            .join(",");
        base_args.extend(["--sub-langs".into(), languages, "--embed-subs".into()]);
    } else if !request.subtitle_languages.is_empty() {
        let languages = validate_subtitle_languages(&request.subtitle_languages)?;
        base_args.push(
            if request.subtitle_mode.as_deref() == Some("automatic") {
                "--no-write-subs"
            } else {
                "--write-subs"
            }
            .into(),
        );
        base_args.push(
            if request.subtitle_mode.as_deref() == Some("manual") {
                "--no-write-auto-subs"
            } else {
                "--write-auto-subs"
            }
            .into(),
        );
        base_args.extend(["--sub-langs".into(), languages]);
        if request.kind != DownloadKind::Audio {
            base_args.push("--embed-subs".into());
        }
    }

    // A finished HLS source with separate video and audio playlists downloads both streams at
    // once in the app; every other source goes through yt-dlp below.
    let native = source_capture
        .pinned
        .as_ref()
        .filter(|_| !live)
        .and_then(|pinned| native_hls::native_streams(pinned, request));
    let mut native_output = match native {
        Some(streams) => {
            crate::logging::record_job(
                job_id,
                crate::logging::Event::info("download.native_hls").detail(format!(
                    "parça={}",
                    streams
                        .iter()
                        .map(native_hls::NativeStream::summary)
                        .collect::<Vec<_>>()
                        .join(" ")
                )),
            );
            Some(native_hls::download_streams(
                &streams,
                &output_plan.workspace,
                request,
                &toolchain,
                control,
                network,
                progress,
            )?)
        }
        None => None,
    };
    let mut completed_files = HashSet::new();
    let mut last_path = None;
    let mut restart_count = 0u32;
    let mut compatibility_retry = false;
    let mut fragment_restarts = 0u32;
    let mut stall_restarts = 0u32;
    loop {
        if let Some(path) = native_output.take() {
            completed_files.insert(path.clone());
            last_path = Some(path);
            break;
        }
        if control.is_cancelled() {
            bail!(crate::i18n::ui(
                "İndirme iptal edildi",
                "The download was cancelled"
            ));
        }
        if control.is_paused() {
            bail!(crate::i18n::ui(
                "İndirme duraklatıldı",
                "The download was paused"
            ));
        }
        let launch_limit = control.speed_limit();
        let mut args = base_args.clone();
        if compatibility_retry {
            args.extend(["--impersonate".into(), "".into()]);
        }
        if launch_limit > 0 {
            args.extend(["--limit-rate".into(), launch_limit.to_string()]);
        }
        args.extend([
            "--concurrent-fragments".into(),
            fragment_concurrency(
                request.connections,
                network.per_host_limit(),
                fragment_restarts,
            ),
        ]);
        progress(TransferProgress {
            processing: false,
            downloaded: completed_file_bytes(&completed_files),
            total: None,
            speed: 0,
            eta: None,
            phase: if restart_count == 0 {
                crate::i18n::ui("Medya indiriliyor", "Downloading media").into()
            } else {
                crate::i18n::ui(
                    "Yeni hız sınırıyla sürdürülüyor",
                    "Continuing with the new speed limit",
                )
                .into()
            },
        });

        // Files finished before this attempt are known good; of the attempt's own files only the
        // streams the tool finished survive a lost fragment.
        let known_good = completed_files.clone();
        let mut running = RunningDownload::spawn_ytdlp(
            yt_dlp,
            &args,
            &mut secrets.config,
            &mut secrets.redactions,
            control,
            network,
            live,
        )?;
        let mut live_gauge = live.then(|| LiveRecordingGauge::new(&output_plan.workspace));
        let outcome = monitor_download(
            &mut running,
            control,
            launch_limit,
            progress,
            &mut completed_files,
            &mut last_path,
            live_gauge.as_mut(),
        )?;
        match outcome {
            MonitorOutcome::RestartForSpeed => {
                restart_count = restart_count.saturating_add(1);
                discard_fragmented_restart(
                    failure_stages.fragmented,
                    &output_plan,
                    &known_good,
                    job_id,
                )?;
                continue;
            }
            MonitorOutcome::Stalled => {
                stall_restarts = stall_restarts.saturating_add(1);
                if stall_restarts > STALL_RESTARTS {
                    crate::bail_code!(crate::error_codes::TRF_001);
                }
                // A killed fragmented run leaves untrustworthy per-fragment resume
                // state: the next run can conclude a never-downloaded fragment is
                // done and publish a hole. Start the fragments again from known-good
                // files instead of resuming inside the chain.
                discard_fragmented_restart(
                    failure_stages.fragmented,
                    &output_plan,
                    &known_good,
                    job_id,
                )?;
                progress(TransferProgress {
                    phase: crate::i18n::ui_format!(
                        "Aktarım ilerlemedi; bağlantı yenileniyor ({0}).",
                        "The transfer stalled; reconnecting ({0}).",
                        stall_restarts
                    ),
                    ..TransferProgress::default()
                });
                continue;
            }
            MonitorOutcome::Paused => crate::bail_code!(crate::error_codes::TRF_009),
            MonitorOutcome::Cancelled => crate::bail_code!(crate::error_codes::TRF_008),
            MonitorOutcome::Finished => {
                progress(TransferProgress {
                    downloaded: completed_file_bytes(&completed_files),
                    phase: crate::i18n::ui(
                        "Canlı kayıt bitirildi; kaydedilen kısım işleniyor",
                        "Live recording finished; processing the recorded part",
                    )
                    .into(),
                    processing: true,
                    ..TransferProgress::default()
                });
                let kept = keep_live_recording(&output_plan, request, &toolchain, control)?;
                completed_files.insert(kept.clone());
                last_path = Some(kept);
                break;
            }
            MonitorOutcome::Exited(status) => {
                if status.success() {
                    if let Some(error) = running.input_error.take() {
                        return Err(error).context(crate::i18n::ui(
                            "Medya isteği gönderilemedi",
                            "Could not send the media request",
                        ));
                    }
                }
                // A skipped fragment leaves a permanent hole: discard this attempt's unfinished files
                // so the next one downloads the affected stream again instead of resuming it.
                let stderr = running.stderr_text();
                if fragment_loss(&stderr) {
                    // The tool's own account of the loss, which the restart below hides.
                    let visible = non_debug_lines(&stderr);
                    let lines = visible.lines().collect::<Vec<_>>();
                    crate::logging::record_job(
                        job_id,
                        crate::logging::Event::warn("download.fragment_loss_output").detail(
                            useful_error(
                                &lines[lines.len().saturating_sub(12)..].join("\n"),
                                &secrets.redactions,
                            ),
                        ),
                    );
                    let discarded = discard_attempt_outputs(&output_plan, &known_good)?;
                    if discarded.0 > 0 {
                        fragment_restarts = fragment_restarts.saturating_add(1);
                        progress(TransferProgress {
                            phase: crate::i18n::ui_format!("Parça atlandı; yarım kalan {0} dosya ({1:.1} MB) atılıp akış baştan indiriliyor (deneme {2}, {3} bağlantı).", "A fragment was skipped; {0} unfinished file(s) ({1:.1} MB) discarded and the stream restarts (attempt {2}, {3} connection(s)).", discarded.0, discarded.1 as f64 / (1024.0 * 1024.0), fragment_restarts + 1, fragment_concurrency(request.connections, network.per_host_limit(), fragment_restarts)),
                            ..TransferProgress::default()
                        });
                        crate::logging::record_job(
                            job_id,
                            crate::logging::Event::warn("download.fragment_skip").detail(format!(
                                "atılan dosya={} bayt={} deneme={}",
                                discarded.0, discarded.1, fragment_restarts
                            )),
                        );
                        if fragment_restarts > FRAGMENT_RESTARTS {
                            crate::bail_code!(
                                crate::error_codes::TRF_011,
                                "{}",
                                crate::i18n::ui_format!("Kaynak parçaları tekrar tekrar düşürdü; akış {0} kez baştan indirildi ve bütünlük sağlanamadı. Kaynağı daha sonra yeniden deneyin.", "The source kept dropping fragments; the stream was restarted {0} time(s) and integrity could not be ensured. Try the source again later.", fragment_restarts)
                            );
                        }
                        continue;
                    }
                }
                if !status.success() {
                    let stderr = running.stderr_text();
                    let error = if network_media_failure(&stderr) {
                        failure_stages.annotate(&stderr)
                    } else {
                        stderr
                    };
                    // generic:impersonate covers only metadata, not media requests.
                    // Retry the identical selection once, preserving context and partial files.
                    if !compatibility_retry && is_forbidden(&error) && completed_files.is_empty() {
                        compatibility_retry = true;
                        progress(TransferProgress {
                            phase: crate::i18n::ui(
                                "Sunucu 403 döndürdü; aynı kaynak tarayıcı uyumlu bağlantıyla bir kez deneniyor",
                                "The server returned 403; retrying the same source once with a browser-compatible connection",
                            )
                            .into(),
                            ..TransferProgress::default()
                        });
                        continue;
                    }
                    let redacted = useful_error(&error, &secrets.redactions);
                    if format_unavailable(&redacted) {
                        crate::bail_code!(
                            crate::error_codes::MED_015,
                            "{}",
                            crate::i18n::ui_format!("Seçilen kalite indirme anında kaynağın listesinde yok; kaynak yenilenmiş olabilir. Kaynağı yeniden çözümleyip var olan bir kaliteyi seçin. {0}", "The selected quality was not in the source's list at download time; the source may have been refreshed. Analyze the source again and pick an available quality. {0}", available_qualities(&info))
                        );
                    }
                    let visible = non_debug_lines(&error);
                    let phase = classify_download_error(
                        &useful_error(&visible, &secrets.redactions),
                        request,
                    )
                    .0;
                    let summary = useful_error(&final_error_text(&visible), &secrets.redactions);
                    crate::logging::record_job(
                        job_id,
                        crate::logging::Event::warn("download.failed_output").detail(redacted),
                    );
                    if site_access_blocked(&visible) {
                        crate::bail_code!(crate::error_codes::MED_008, "{summary}");
                    }
                    bail!(
                        "{}",
                        crate::i18n::ui_format!("{0} aşamasında medya indirmesi başarısız (uyumluluk denemesi: {1}/1): {2}", "Media download failed at the {0} stage (compatibility attempt: {1}/1): {2}", failure_stage_label(phase), u8::from(compatibility_retry), summary)
                    );
                }
                break;
            }
        }
    }

    for path in &completed_files {
        let checked = output_plan.checked_output(path)?;
        if !postprocess_selected_streams(&checked, request, &info, &toolchain, control, network)? {
            verify_media_output(&checked, request, &info, &toolchain, control)?;
        }
    }
    let result_path = output_plan.publish(
        output,
        request.playlist,
        &completed_files,
        last_path.as_deref(),
    )?;
    let bytes = path_bytes(&result_path)?;
    progress(TransferProgress {
        processing: false,
        downloaded: bytes,
        total: Some(bytes),
        speed: 0,
        eta: Some(0),
        phase: crate::i18n::ui("Tamamlandı", "Completed").into(),
    });
    Ok(TransferResult {
        path: result_path,
        bytes,
        warning: None,
    })
}

pub(crate) fn verify_external_result(
    paths: &AppPaths,
    request: &AddRequest,
    path: &Path,
    streams: &[crate::browser_transfer::StreamFile],
    source_duration: Option<f64>,
    full_verification: bool,
) -> Result<u64> {
    if !path.is_file() {
        bail!(crate::i18n::ui(
            "Doğrulanacak tarayıcı medya çıktısı bulunamadı",
            "The browser media output to verify was not found"
        ));
    }
    validate_options(request)?;
    let control = TransferControl::default();
    let toolchain = tools::verified_toolchain(paths, &control)?;
    // The source may be accessible only inside the authorized browser document.
    // Stream identities/languages were matched there and checked by the muxer;
    // verify the local result without fetching the manifest from another engine.
    let info = MediaInfo {
        duration: source_duration,
        formats: streams
            .iter()
            .filter_map(|stream| {
                Some(MediaFormat {
                    id: stream.format_id.clone()?,
                    video_codec: (stream.role == "audio").then(|| "none".into()),
                    audio_codec: (stream.role == "video").then(|| "none".into()),
                    language: stream.language.clone(),
                    ..MediaFormat::default()
                })
            })
            .collect(),
        ..MediaInfo::default()
    };
    let mut verification_request = request.clone();
    verification_request.full_verification = full_verification;
    verify_media_output(path, &verification_request, &info, &toolchain, &control)?;
    path_bytes(path)
}

pub(crate) fn mux_browser_streams(
    paths: &AppPaths,
    request: &AddRequest,
    streams: &[crate::browser_transfer::StreamFile],
    output: &Path,
    control: &TransferControl,
) -> Result<()> {
    validate_options(request)?;
    if output.exists() {
        bail!(crate::i18n::ui(
            "Tarayıcı medya hedefi zaten var; mevcut kullanıcı dosyasının üzerine yazılmadı",
            "The browser media target already exists; the existing user file was not overwritten"
        ));
    }
    for stream in streams {
        if !stream.path.is_file() {
            bail!(crate::i18n::ui(
                "Tarayıcı medya akışı çalışma dosyasında bulunamadı",
                "The browser media stream was not found in the working file"
            ));
        }
        if !matches!(stream.role.as_str(), "video" | "audio" | "subtitle") {
            bail!(crate::i18n::ui(
                "Tarayıcı bilinmeyen bir medya akışı rolü gönderdi",
                "The browser sent an unknown media stream role"
            ));
        }
    }
    let videos = streams
        .iter()
        .filter(|stream| stream.role == "video")
        .collect::<Vec<_>>();
    let audios = streams
        .iter()
        .filter(|stream| stream.role == "audio")
        .collect::<Vec<_>>();
    let subtitles = streams
        .iter()
        .filter(|stream| stream.role == "subtitle")
        .collect::<Vec<_>>();
    if request.kind != DownloadKind::Audio && videos.len() != 1 {
        bail!(crate::i18n::ui(
            "Tarayıcı aktarımında tam bir seçili görüntü akışı bulunamadı",
            "The browser transfer does not contain exactly one selected video stream"
        ));
    }
    if request.kind == DownloadKind::Audio && !videos.is_empty() {
        bail!(crate::i18n::ui(
            "Tarayıcı yalnız ses aktarımında görüntü akışı gönderdi",
            "The browser sent a video stream in an audio-only transfer"
        ));
    }
    let expected_audio = if !request.audio_tracks.is_empty() {
        request.audio_tracks.len()
    } else if request.audio_format_id.is_some()
        || request.audio_language.is_some()
        || request.kind == DownloadKind::Audio
        || request.expected_audio == Some(true)
    {
        1
    } else {
        audios.len()
    };
    if audios.len() != expected_audio {
        bail!(crate::i18n::ui(
            "Tarayıcı aktarımında seçilen ses akışlarının tamamı bulunamadı",
            "Not all selected audio streams were found in the browser transfer"
        ));
    }
    for (index, stream) in audios.iter().enumerate() {
        let expected_id = request
            .audio_tracks
            .get(index)
            .map(|selection| selection.id.as_str())
            .or(request.audio_format_id.as_deref());
        if expected_id.is_some() && stream.format_id.as_deref() != expected_id {
            bail!(crate::i18n::ui(
                "Tarayıcı ses akışı seçilen format kimliğiyle eşleşmiyor",
                "The browser audio stream does not match the selected format id"
            ));
        }
        let expected_language = request
            .audio_tracks
            .get(index)
            .and_then(|selection| selection.language.as_deref())
            .or(request.audio_language.as_deref());
        if expected_language.is_some()
            && stream.language.as_deref().is_none_or(|actual| {
                language_key(actual) != language_key(expected_language.unwrap())
            })
        {
            bail!(crate::i18n::ui(
                "Tarayıcı ses akışı seçilen dille eşleşmiyor",
                "The browser audio stream does not match the selected language"
            ));
        }
    }
    if let Some(id) = &request.video_format_id {
        let video = videos.first().context(crate::i18n::ui(
            "Tarayıcı aktarımında seçilen görüntü akışı bulunamadı",
            "The selected video stream was not found in the browser transfer",
        ))?;
        if video.format_id.as_deref() != Some(id.as_str()) {
            bail!(crate::i18n::ui(
                "Tarayıcı görüntü akışı seçilen format kimliğiyle eşleşmiyor",
                "The browser video stream does not match the selected format id"
            ));
        }
    }
    let expected_subtitles = if !request.external_subtitles.is_empty() {
        request.external_subtitles.len()
    } else if !request.subtitle_tracks.is_empty() {
        request.subtitle_tracks.len()
    } else {
        request.subtitle_languages.len()
    };
    if subtitles.len() != expected_subtitles {
        bail!(crate::i18n::ui(
            "Tarayıcı aktarımında seçilen altyazı akışlarının tamamı bulunamadı",
            "Not all selected subtitle streams were found in the browser transfer"
        ));
    }

    let tools = tools::verified_toolchain(paths, control)?;
    let parent = output.parent().context(crate::i18n::ui(
        "Tarayıcı medya hedef klasörü eksik",
        "The browser media target folder is missing",
    ))?;
    fs::create_dir_all(parent)?;
    for subtitle in &subtitles {
        if let Ok(text) = fs::read_to_string(&subtitle.path) {
            if let Some(playable) = without_unmuxable_cues(&text) {
                fs::write(&subtitle.path, playable)?;
            }
        }
    }
    let staged = parent.join(format!(".browser-mux-{}.tmp", uuid::Uuid::new_v4()));
    let args = browser_mux_args(request, streams, &staged);
    let result = run_capture(
        &tools.ffmpeg,
        &args,
        b"",
        Duration::from_secs(10 * 60),
        control,
    )?;
    if !result.status.success() || !result.stderr.trim().is_empty() {
        let _ = fs::remove_file(&staged);
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Tarayıcı medya akışları kayıpsız birleştirilemedi: {0}",
                "The browser media streams could not be merged losslessly: {0}",
                bounded_tail(&result.stderr, 4096)
            )
        );
    }
    if let Err(error) = fs::rename(&staged, output) {
        let _ = fs::remove_file(&staged);
        return Err(error).context(crate::i18n::ui(
            "Tarayıcı medya çıktısı hedefe alınamadı",
            "Could not move the browser media output to its target",
        ));
    }
    Ok(())
}

/// FFmpeg arguments that copy the browser's stream files into one container.
fn browser_mux_args(
    request: &AddRequest,
    streams: &[crate::browser_transfer::StreamFile],
    staged: &Path,
) -> Vec<String> {
    let audios = streams
        .iter()
        .filter(|stream| stream.role == "audio")
        .collect::<Vec<_>>();
    let subtitle_count = streams
        .iter()
        .filter(|stream| stream.role == "subtitle")
        .count();
    let mut args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-nostdin".into(),
    ];
    // The whitelist is an input option: every input needs its own.
    for stream in streams {
        args.extend([
            "-protocol_whitelist".into(),
            "file,crypto,data".into(),
            "-i".into(),
            stream.path.to_string_lossy().into_owned(),
        ]);
    }
    for (index, stream) in streams.iter().enumerate() {
        let specifier = match stream.role.as_str() {
            "video" => "v:0",
            "audio" => "a:0",
            "subtitle" => "s:0",
            _ => unreachable!(),
        };
        args.extend(["-map".into(), format!("{index}:{specifier}")]);
    }
    args.extend(["-c:v".into(), "copy".into(), "-c:a".into(), "copy".into()]);
    if subtitle_count > 0 {
        args.extend([
            "-c:s".into(),
            if request.container.as_deref() == Some("mp4") {
                "mov_text"
            } else {
                "srt"
            }
            .into(),
        ]);
    }
    for (index, audio) in audios.iter().enumerate() {
        let language = request
            .audio_tracks
            .get(index)
            .and_then(|selection| selection.language.as_deref())
            .or(request.audio_language.as_deref())
            .or(audio.language.as_deref());
        if let Some(language) = language {
            args.extend([
                format!("-metadata:s:a:{index}"),
                format!(
                    "language={}",
                    tag_language(language, request.container.as_deref())
                ),
            ]);
        }
        args.extend([
            format!("-disposition:a:{index}"),
            if index == 0 { "default" } else { "0" }.into(),
        ]);
    }
    for index in 0..subtitle_count {
        let (language, is_default) = if let Some(selected) = request.external_subtitles.get(index) {
            (Some(selected.language.as_str()), selected.is_default)
        } else if let Some(selected) = request.subtitle_tracks.get(index) {
            (Some(selected.language.as_str()), false)
        } else {
            (
                request.subtitle_languages.get(index).map(String::as_str),
                false,
            )
        };
        if let Some(language) = language {
            args.extend([
                format!("-metadata:s:s:{index}"),
                format!(
                    "language={}",
                    tag_language(language, request.container.as_deref())
                ),
            ]);
        }
        args.extend([
            format!("-disposition:s:{index}"),
            if is_default { "default" } else { "0" }.into(),
        ]);
    }
    let muxer = match request.container.as_deref() {
        None | Some("mkv") => "matroska",
        Some(container) => container,
    };
    args.extend(["-f".into(), muxer.into()]);
    args.push(staged.to_string_lossy().into_owned());
    args
}

/// The request as it is downloaded from the stream's beginning. The picker chose its quality
/// from the live formats, whose ids the beginning's formats do not share, so only the height
/// carries over.
fn live_from_start_request(request: &AddRequest) -> AddRequest {
    let mut resolved = request.clone();
    resolved.format_id = None;
    resolved.video_format_id = None;
    resolved.audio_format_id = None;
    resolved.audio_language = None;
    resolved.audio_tracks.clear();
    resolved
}

/// Whether yt-dlp answered with formats that begin at a live stream's start: it keeps only
/// those when asked for them and the site offers them.
fn from_start_formats(inspection: &Value) -> bool {
    inspection["formats"].as_array().is_some_and(|formats| {
        formats
            .iter()
            .any(|format| format["is_from_start"].as_bool() == Some(true))
    })
}

/// The partial files of a stopped live recording: the tool's `.part` outputs, never its
/// per-fragment `.part-FragN` pieces.
fn live_recording_parts(workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut parts = Vec::new();
    for entry in fs::read_dir(workspace)? {
        let path = entry?.path();
        // `<part>-Frag<n>.part` is a fragment yt-dlp is still fetching for `<part>`.
        let is_part = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("part"))
            && !path.to_string_lossy().contains(".part-Frag");
        if is_part && fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0) {
            parts.push(path);
        }
    }
    parts.sort();
    Ok(parts)
}

/// FFmpeg arguments that copy the audio and video of every part into one file. Live HLS
/// carries timed metadata streams the MP4 container cannot hold, so only audio and video
/// are mapped.
fn live_recording_remux_args(
    parts: &[PathBuf],
    output: &Path,
    container: &str,
    keep_seconds: Option<f64>,
) -> Vec<String> {
    let mut args: Vec<String> = ["-y", "-v", "error", "-nostdin"]
        .into_iter()
        .map(str::to_string)
        .collect();
    for part in parts {
        args.extend(["-i".into(), part.to_string_lossy().into_owned()]);
    }
    for index in 0..parts.len() {
        args.extend([
            "-map".into(),
            format!("{index}:v?"),
            "-map".into(),
            format!("{index}:a?"),
        ]);
    }
    if let Some(seconds) = keep_seconds {
        args.extend(["-t".into(), format!("{seconds:.3}")]);
    }
    args.extend(["-c".into(), "copy".into()]);
    if container == "mp4" {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.extend([
        "-f".into(),
        if container == "mkv" {
            "matroska"
        } else {
            "mp4"
        }
        .into(),
        output.to_string_lossy().into_owned(),
    ]);
    args
}

/// How long every stream of `file` runs, from the earliest start to the end of the shortest.
fn recorded_seconds(
    toolchain: &tools::VerifiedTools,
    file: &Path,
    control: &TransferControl,
) -> Result<Option<f64>> {
    let result = run_capture(
        &toolchain.ffprobe,
        &[
            "-v".into(),
            "error".into(),
            "-show_streams".into(),
            "-of".into(),
            "json".into(),
            file.to_string_lossy().into_owned(),
        ],
        b"",
        Duration::from_secs(60),
        control,
    )?;
    let probe: Value = serde_json::from_slice(&result.stdout).unwrap_or_default();
    let spans: Vec<(f64, f64)> = probe["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(stream_time_span)
        .collect();
    let start = spans.iter().map(|span| span.0).reduce(f64::min);
    let end = spans.iter().map(|span| span.1).reduce(f64::min);
    Ok(start.zip(end).map(|(start, end)| end - start))
}

/// Turns what a stopped live recording wrote into one finished file in the workspace.
fn keep_live_recording(
    plan: &OutputPlan,
    request: &AddRequest,
    toolchain: &tools::VerifiedTools,
    control: &TransferControl,
) -> Result<PathBuf> {
    // FFmpeg needs the extended form of the nested workspace path, which can exceed MAX_PATH.
    let workspace = plan.workspace.canonicalize()?;
    let parts = live_recording_parts(&workspace)?;
    if parts.is_empty() {
        bail!(crate::i18n::ui(
            "Canlı kayıt bitirildi ama henüz kaydedilmiş bir bölüm yok",
            "The live recording was finished, but nothing had been recorded yet"
        ));
    }
    let container = match request.container.as_deref() {
        Some("mkv") => "mkv",
        _ => "mp4",
    };
    // The parts end at different moments (video and audio download apart), and a recorder
    // stopped by force leaves its last frame incomplete. Their own durations are unreliable
    // (fragmented parts carry the stream's absolute timestamps), so they are joined first and
    // the joined file is cut to its shortest stream, minus the final second.
    let joined = workspace.join("joined.mkv");
    remux_live_recording(
        toolchain,
        &live_recording_remux_args(&parts, &joined, "mkv", None),
        &joined,
        control,
    )?;
    let keep_seconds = recorded_seconds(toolchain, &joined, control)?
        .map(|seconds| seconds - 1.0)
        .filter(|seconds| *seconds > 1.0);
    let output = workspace.join(format!("media.{container}"));
    remux_live_recording(
        toolchain,
        &live_recording_remux_args(
            std::slice::from_ref(&joined),
            &output,
            container,
            keep_seconds,
        ),
        &output,
        control,
    )?;
    Ok(output)
}

fn remux_live_recording(
    toolchain: &tools::VerifiedTools,
    args: &[String],
    output: &Path,
    control: &TransferControl,
) -> Result<()> {
    let result = run_capture(
        &toolchain.ffmpeg,
        args,
        b"",
        Duration::from_secs(60 * 60),
        control,
    );
    let written = result.as_ref().is_ok_and(|result| result.status.success())
        && fs::metadata(output).is_ok_and(|m| m.len() > 0);
    if !written {
        // A half-written file under the final name would pass as already downloaded on retry.
        let _ = fs::remove_file(output);
    }
    let result = result?;
    if !written {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Canlı kaydın kaydedilen kısmı dosyaya yazılamadı: {0}",
                "The recorded part of the live stream could not be written to a file: {0}",
                result.stderr.trim()
            )
        );
    }
    Ok(())
}

/// Arguments that make yt-dlp download from a captured extractor document. Only the pinned
/// bundle needs `--enable-file-urls`, because its manifests are local files; an unpinned
/// document is built from remote site data and must never be able to open a file or share.
fn load_info_args(info_path: &Path, pinned: bool) -> Vec<String> {
    let mut args = Vec::with_capacity(3);
    if pinned {
        args.push("--enable-file-urls".into());
    }
    args.extend([
        "--load-info-json".into(),
        info_path.to_string_lossy().into_owned(),
    ]);
    args
}

fn common_args(deno: &Path, ffmpeg: &Path, playlist: bool) -> Vec<String> {
    let ffmpeg_dir = ffmpeg.parent().unwrap_or(ffmpeg);
    vec![
        "--ignore-config".into(),
        "--no-plugin-dirs".into(),
        "--no-exec".into(),
        // Without it the frozen tool writes stdout/stderr in the Windows ANSI code page and drops
        // everything outside it, while every reader here decodes UTF-8.
        "--encoding".into(),
        "utf-8".into(),
        "--config-locations".into(),
        "-".into(),
        "--no-cache-dir".into(),
        "--extractor-args".into(),
        "generic:impersonate".into(),
        "--ffmpeg-location".into(),
        ffmpeg_dir.to_string_lossy().into_owned(),
        "--js-runtimes".into(),
        format!("deno:{}", deno.to_string_lossy()),
        if playlist {
            "--yes-playlist".into()
        } else {
            "--no-playlist".into()
        },
    ]
}

fn is_forbidden(error: &str) -> bool {
    error.contains("HTTP Error 403") || error.contains("HTTP Error: 403")
}

/// Phrases of yt-dlp's final `ERROR:` line when the origin refuses the media for this viewer
/// (region, removal, privacy, membership, sign-in) as opposed to a transport or tool failure.
const SITE_ACCESS_PHRASES: &[&str] = &[
    "video unavailable",
    "not available in your country",
    "geo restrict",
    "geo-restrict",
    "private video",
    "video is private",
    "has been removed",
    "has been terminated",
    "members-only",
    "members only",
    "join this channel",
    "sign in",
    "log in",
    "login required",
    "age-restricted",
    "confirm your age",
];

fn site_access_blocked(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_start();
        if !line.starts_with("ERROR:") {
            return false;
        }
        let lower = line.to_ascii_lowercase();
        SITE_ACCESS_PHRASES
            .iter()
            .any(|phrase| lower.contains(phrase))
    })
}

/// The tool's output without its `[debug]` trace (version header, argument echo, request log).
fn non_debug_lines(text: &str) -> String {
    text.replace('\r', "\n")
        .lines()
        .filter(|line| !line.trim_start().starts_with("[debug]"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The failure a user can act on: from the last `ERROR:` line to the end, bounded. The verbose
/// trace around it is kept in the log only.
fn final_error_text(text: &str) -> String {
    let text = text.trim();
    let start = text.rfind("ERROR:").unwrap_or(0);
    bounded_tail(text[start..].trim(), 2048)
}

/// yt-dlp drops DRM formats and, when nothing else remains, ends with an `ERROR: ... This video
/// is DRM protected` line instead of JSON; a per-format warning about skipped formats is not it.
fn drm_protected(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("ERROR:") && line.to_ascii_lowercase().contains("drm protected")
    })
}

/// yt-dlp reports a hole in the stream instead of failing the download: "[download] fragment not
/// found; Skipping fragment 5". A resumed `.part` file keeps that hole forever, so the attempt
/// that produced it must be thrown away rather than continued.
/// A fragmented (HLS/DASH) run killed mid-chain must not resume its fragment state:
/// observed yt-dlp resume states can treat a never-fetched fragment as done. Discard
/// this attempt's own outputs back to the known-good set; completed playlist entries
/// (`known_good`) are kept, so multi-entry playlists lose only the unfinished entry.
fn discard_fragmented_restart(
    fragmented: bool,
    output_plan: &OutputPlan,
    known_good: &HashSet<PathBuf>,
    job_id: &str,
) -> Result<()> {
    if !fragmented {
        return Ok(());
    }
    let discarded = discard_attempt_outputs(output_plan, known_good)?;
    if discarded.0 > 0 {
        crate::logging::record_job(
            job_id,
            crate::logging::Event::warn("download.fragment_restart_discard")
                .detail(format!("atılan dosya={} bayt={}", discarded.0, discarded.1)),
        );
    }
    Ok(())
}

fn fragment_loss(stderr: &str) -> bool {
    if stderr.contains("Skipping fragment")
        || stderr.contains("Skipping already downloaded fragment")
    {
        return true;
    }
    // A stream given up names its fragment: "fragment 6 not found, unable to continue".
    if stderr.match_indices("fragment").any(|(index, word)| {
        stderr[index + word.len()..]
            .trim_start_matches(|c: char| c == ' ' || c.is_ascii_digit())
            .starts_with("not found")
    }) {
        return true;
    }
    // The field failure: the fragment temp file disappeared under the downloader.
    stderr.contains("No such file or directory") && stderr.contains(".part-Frag")
}

/// The distinct video heights the analyzed source still offers, so a failure that names the
/// selected quality can also say what the user may pick instead.
fn available_qualities(info: &MediaInfo) -> String {
    let mut heights: Vec<u32> = info
        .formats
        .iter()
        .filter_map(|format| format.height)
        .filter(|height| *height > 0)
        .collect();
    heights.sort_unstable();
    heights.dedup();
    if heights.is_empty() {
        return crate::i18n::ui(
            "Analizde çözünürlük bilgisi yok.",
            "The analysis has no resolution information.",
        )
        .into();
    }
    let list = heights
        .iter()
        .map(|height| format!("{height}p"))
        .collect::<Vec<_>>()
        .join(", ");
    crate::i18n::ui_format!(
        "Analizde görülen çözünürlükler: {list}",
        "Resolutions seen in the analysis: {list}",
        list = list
    )
}

/// yt-dlp could not satisfy the requested selection: the manifest resolved for the download no
/// longer offers the quality (or track) that the analysis had shown. Reported with its own code
/// because the fix is "analyze again and pick an available quality", not "retry".
fn format_unavailable(error: &str) -> bool {
    error.contains("Requested format is not available")
        || error.contains("requested format not available")
}

/// Fragment parallelism cannot exceed the network governor's per-host tunnel
/// bound; requesting more only produced proxy 503s and fragment aborts.
/// Each restart after a lost fragment halves the parallelism: fewer requests ease a source that
/// drops them, while the restarted stream still downloads in parallel.
fn fragment_concurrency(requested: Option<u8>, per_host_limit: u8, restarts: u32) -> String {
    // One tunnel of the per-host bound stays free. A slow fragment keeps its connection open while
    // the downloader opens a fresh one for a retry; a job that holds the whole bound turns that
    // retry into a local 503 and then into a lost fragment.
    let headroom = per_host_limit.saturating_sub(1).max(1);
    requested
        .unwrap_or(4)
        .clamp(1, 16)
        .min(headroom)
        .checked_shr(restarts)
        .unwrap_or(0)
        .max(1)
        .to_string()
}

fn network_media_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("http error")
        || lower.contains("forbidden")
        || lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("resolve host")
        || lower.contains("name or service not known")
        || lower.contains("dns")
        || lower.contains("timed out")
        || lower.contains("timeout")
        // Local proxy tunnel saturation/failure; transient by construction.
        || lower.contains("tunnel connection failed")
        || lower.contains("unable to connect to proxy")
        || lower.contains("503 service unavailable")
}

/// A stored failure text lowercased for matching, read alike whatever interface language wrote
/// it: every language's name of a failure stage becomes the English one, and each stable code
/// the text carries adds its English message.
pub(crate) fn failure_reading(error: &str) -> String {
    let mut lower = error.to_ascii_lowercase();
    for (tr, en) in FAILURE_STAGES {
        for wording in crate::i18n::wordings(tr, en) {
            lower = lower.replace(&wording.to_ascii_lowercase(), &en.to_ascii_lowercase());
        }
    }
    for entry in crate::error_codes::TABLE
        .iter()
        .filter(|entry| error.contains(entry.code.0))
    {
        lower.push('\n');
        lower.push_str(&entry.message_en.to_ascii_lowercase());
    }
    lower
}

pub(crate) fn classify_download_error(
    error: &str,
    request: &AddRequest,
) -> (&'static str, &'static str) {
    let lower = failure_reading(error);
    let kind = if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        "tls"
    } else if lower.contains("resolve host")
        || lower.contains("name or service not known")
        || lower.contains("dns")
    {
        "dns"
    } else if [
        "http 401",
        "http 403",
        "http error 401",
        "http error 403",
        "http error: 401",
        "http error: 403",
        "forbidden",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        "access_denied"
    } else if ["http 404", "http error 404", "http error: 404"]
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        "not_found"
    } else if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("zaman aşımı")
    {
        "timeout"
    } else if lower.contains("doğrulama")
        || lower.contains("verification")
        || lower.contains("checksum")
    {
        "verification"
    } else {
        "other"
    };
    // An access/TLS failure must not erase which part of the media graph failed.
    let phase = if let Some(phase) = marked_download_failure_phase(&lower) {
        phase
    } else if lower.contains("subtitle") || lower.contains("altyazı") {
        "altyazı"
    } else if lower.contains("initialization")
        || lower.contains("init segment")
        || lower.contains("başlatma parçası")
    {
        "başlatma parçası"
    } else if lower.contains("fragment")
        || lower.contains("segment")
        || lower.contains("medya parçası")
    {
        "medya parçası"
    } else if lower.contains("seçilen ses")
        || lower.contains("selected audio")
        || ((!request.audio_tracks.is_empty()
            || request.audio_format_id.is_some()
            || request.expected_audio == Some(true))
            && lower.contains("audio"))
    {
        "seçilen ses"
    } else if lower.contains("alt liste")
        || lower.contains("sub-playlist")
        || lower.contains("representation")
        || lower.contains("variant playlist")
    {
        "alt liste"
    } else if lower.contains("medya analizi başarısız")
        || lower.contains("media analysis failed")
        || lower.contains("media extractor failed")
        || lower.contains("discovery")
    {
        if media_manifest_url(&request.url) {
            "ana liste"
        } else {
            "keşif"
        }
    } else if lower.contains("ana liste")
        || lower.contains("main playlist")
        || lower.contains("m3u8")
        || lower.contains("mpd")
        || lower.contains("manifest")
    {
        "ana liste"
    } else if lower.contains("doğrulama")
        || lower.contains("verification")
        || lower.contains("checksum")
    {
        "doğrulama"
    } else {
        "medya kaynağı"
    };
    (phase, kind)
}

const MAX_FAILURE_STAGE_URLS: usize = 256;

#[derive(Default)]
struct DownloadFailureStages {
    urls: Vec<(String, &'static str)>,
    /// A selected stream is delivered as HLS/DASH fragments, whatever the page address looks like.
    fragmented: bool,
}

/// Whether any stream the selector picks is fragmented (HLS/DASH), judged from the extractor's
/// own protocol for the selected formats; the address suffix only covers a bare manifest URL.
fn selection_is_fragmented(inspection: &Value, request: &AddRequest, selector: &str) -> bool {
    if media_manifest_url(&request.url) {
        return true;
    }
    let roots: Vec<&Value> = match inspection.get("entries").and_then(Value::as_array) {
        Some(entries) => entries.iter().filter(|entry| !entry.is_null()).collect(),
        None => vec![inspection],
    };
    roots.into_iter().any(|root| {
        let Some(ids) = media_info_from_json(root)
            .ok()
            .and_then(|info| selected_format_ids(selector, &info).ok())
        else {
            return false;
        };
        // A result without a format list is its own format, as in `parse_formats`.
        let formats = match root.get("formats").and_then(Value::as_array) {
            Some(formats) => formats.iter().collect(),
            None => vec![root],
        };
        formats.into_iter().any(|format| {
            format
                .get("format_id")
                .and_then(Value::as_str)
                .is_some_and(|id| ids.iter().any(|selected| selected == id))
                && (format
                    .get("protocol")
                    .and_then(Value::as_str)
                    .is_some_and(|protocol| protocol.contains("m3u8") || protocol.contains("dash"))
                    || format
                        .get("fragments")
                        .and_then(Value::as_array)
                        .is_some_and(|fragments| !fragments.is_empty()))
        })
    })
}

impl DownloadFailureStages {
    fn from_inspection(inspection: &Value, request: &AddRequest, selector: &str) -> Self {
        let mut stages = Self {
            fragmented: selection_is_fragmented(inspection, request, selector),
            ..Self::default()
        };
        if media_manifest_url(&request.url) {
            if let Ok(url) = Url::parse(&request.url) {
                stages.add(url, "manifest_root");
            }
        }

        if let Some(entries) = inspection.get("entries").and_then(Value::as_array) {
            for entry in entries.iter().filter(|entry| !entry.is_null()) {
                collect_failure_stages(&mut stages, entry, request, selector);
            }
        } else {
            collect_failure_stages(&mut stages, inspection, request, selector);
        }
        stages
    }

    fn add(&mut self, url: Url, phase: &'static str) {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return;
        }
        let url: String = url.into();
        if let Some((_, previous)) = self.urls.iter_mut().find(|(known, _)| known == &url) {
            if failure_stage_rank(phase) > failure_stage_rank(previous) {
                *previous = phase;
            }
            return;
        }
        if self.urls.len() < MAX_FAILURE_STAGE_URLS {
            self.urls.push((url, phase));
        }
    }

    fn annotate(&self, error: &str) -> String {
        let mut matched = None;
        for (url, phase) in &self.urls {
            if let Some(position) = error.rfind(url) {
                if matched.is_none_or(|(last, _)| position > last) {
                    matched = Some((position, *phase));
                }
            }
        }
        matched.map_or_else(
            || error.to_string(),
            |(_, phase)| format!("{error}\n[ssdownload-stage={phase}]"),
        )
    }
}

fn collect_failure_stages(
    stages: &mut DownloadFailureStages,
    root: &Value,
    request: &AddRequest,
    selector: &str,
) {
    let Ok(info) = media_info_from_json(root) else {
        return;
    };
    let Ok(selected_ids) = selected_format_ids(selector, &info) else {
        return;
    };
    let Some(formats) = root.get("formats").and_then(Value::as_array) else {
        return;
    };
    let source = Url::parse(&request.url).ok();
    for format_id in selected_ids {
        let Some(format) = formats.iter().find(|format| {
            format.get("format_id").and_then(Value::as_str) == Some(format_id.as_str())
        }) else {
            continue;
        };
        let audio = format.get("vcodec").and_then(Value::as_str) == Some("none");
        let format_url = format.get("url").and_then(Value::as_str).and_then(|value| {
            Url::parse(value)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))
        });
        if let Some(url) = format_url.as_ref() {
            if audio {
                stages.add(url.clone(), "audio");
            } else if media_manifest_url(url.as_str()) && source.as_ref() != Some(url) {
                stages.add(url.clone(), "manifest_child");
            }
        }

        let fragment_base = format
            .get("fragment_base_url")
            .and_then(Value::as_str)
            .or_else(|| format.get("url").and_then(Value::as_str))
            .and_then(|value| Url::parse(value).ok());
        let Some(fragments) = format.get("fragments").and_then(Value::as_array) else {
            continue;
        };
        for fragment in fragments {
            let Some(url) = dash_fragment_url(fragment, fragment_base.as_ref()) else {
                continue;
            };
            let phase = if audio {
                "audio"
            } else if fragment
                .get("initialization")
                .or_else(|| fragment.get("is_initialization"))
                .or_else(|| fragment.get("init"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                "initialization"
            } else {
                "media_segment"
            };
            stages.add(url, phase);
        }
    }
}

fn failure_stage_rank(phase: &str) -> u8 {
    match phase {
        "manifest_root" => 0,
        "manifest_child" => 1,
        "media_segment" => 2,
        "initialization" => 3,
        "audio" => 4,
        _ => 0,
    }
}

fn media_manifest_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .and_then(|url| {
            url.path()
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
        })
        .is_some_and(|extension| matches!(extension.as_str(), "m3u8" | "mpd"))
}

/// The failure stages [`classify_download_error`] returns. The Turkish wording is also the
/// stable stage key the diagnostics match; only the display text is localized.
const FAILURE_STAGES: [(&str, &str); 9] = [
    ("keşif", "discovery"),
    ("ana liste", "main playlist"),
    ("alt liste", "sub-playlist"),
    ("seçilen ses", "selected audio"),
    ("altyazı", "subtitle"),
    ("başlatma parçası", "initialization segment"),
    ("medya parçası", "media segment"),
    ("doğrulama", "verification"),
    ("medya kaynağı", "media source"),
];

/// The wording of a failure stage returned by [`classify_download_error`].
fn failure_stage_label(stage: &str) -> &'static str {
    for (tr, en) in FAILURE_STAGES {
        if tr == stage {
            return crate::i18n::ui(tr, en);
        }
    }
    crate::i18n::ui("medya kaynağı", "media source")
}

fn marked_download_failure_phase(error: &str) -> Option<&'static str> {
    [
        ("[ssdownload-stage=manifest_root]", "ana liste"),
        ("[ssdownload-stage=manifest_child]", "alt liste"),
        ("[ssdownload-stage=audio]", "seçilen ses"),
        ("[ssdownload-stage=initialization]", "başlatma parçası"),
        ("[ssdownload-stage=media_segment]", "medya parçası"),
    ]
    .into_iter()
    .find_map(|(marker, phase)| error.contains(marker).then_some(phase))
}

/// ISO 639-2 form of a language tag. The MP4 (mov) muxer writes only three-letter codes and
/// silently drops anything else, so a selected "tr" never reaches the file.
fn iso639_2(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let base = lower.split(['-', '_']).next().unwrap_or("");
    match base {
        "tr" | "tur" => "tur",
        "en" | "eng" => "eng",
        "de" | "deu" | "ger" => "deu",
        "fr" | "fra" | "fre" => "fra",
        "es" | "spa" => "spa",
        "it" | "ita" => "ita",
        "pt" | "por" => "por",
        "ja" | "jpn" => "jpn",
        "ko" | "kor" => "kor",
        "zh" | "zho" | "chi" => "zho",
        "ar" | "ara" => "ara",
        "ru" | "rus" => "rus",
        "nl" | "nld" | "dut" => "nld",
        "pl" | "pol" => "pol",
        "sv" | "swe" => "swe",
        "no" | "nor" => "nor",
        "da" | "dan" => "dan",
        "fi" | "fin" => "fin",
        "el" | "ell" | "gre" => "ell",
        "he" | "heb" => "heb",
        "hi" | "hin" => "hin",
        "cs" | "ces" | "cze" => "ces",
        "hu" | "hun" => "hun",
        "ro" | "ron" | "rum" => "ron",
        "uk" | "ukr" => "ukr",
        "bg" | "bul" => "bul",
        "fa" | "fas" | "per" => "fas",
        "id" | "ind" => "ind",
        "vi" | "vie" => "vie",
        "th" | "tha" => "tha",
        "az" | "aze" => "aze",
        "sr" | "srp" => "srp",
        "hr" | "hrv" => "hrv",
        "sk" | "slk" | "slo" => "slk",
        "sl" | "slv" => "slv",
        "lt" | "lit" => "lit",
        "lv" | "lav" => "lav",
        "et" | "est" => "est",
        "hy" | "hye" | "arm" => "hye",
        "ka" | "kat" | "geo" => "kat",
        "af" | "afr" => "afr",
        "sq" | "sqi" | "alb" => "sqi",
        "am" | "amh" => "amh",
        "eu" | "eus" | "baq" => "eus",
        "be" | "bel" => "bel",
        "bn" | "ben" => "ben",
        "bs" | "bos" => "bos",
        "ca" | "cat" => "cat",
        "cy" | "cym" | "wel" => "cym",
        "eo" | "epo" => "epo",
        "ga" | "gle" => "gle",
        "gl" | "glg" => "glg",
        "gu" | "guj" => "guj",
        "is" | "isl" | "ice" => "isl",
        "kk" | "kaz" => "kaz",
        "km" | "khm" => "khm",
        "kn" | "kan" => "kan",
        "ky" | "kir" => "kir",
        "la" | "lat" => "lat",
        "lo" | "lao" => "lao",
        "mk" | "mkd" | "mac" => "mkd",
        "ml" | "mal" => "mal",
        "mn" | "mon" => "mon",
        "mr" | "mar" => "mar",
        "ms" | "msa" | "may" => "msa",
        "mt" | "mlt" => "mlt",
        "my" | "mya" | "bur" => "mya",
        "nb" | "nob" => "nob",
        "ne" | "nep" => "nep",
        "nn" | "nno" => "nno",
        "pa" | "pan" => "pan",
        "ps" | "pus" => "pus",
        "si" | "sin" => "sin",
        "so" | "som" => "som",
        "sw" | "swa" => "swa",
        "ta" | "tam" => "tam",
        "te" | "tel" => "tel",
        "tl" | "tgl" => "tgl",
        "ur" | "urd" => "urd",
        "uz" | "uzb" => "uzb",
        // Deprecated two-letter codes that extractors still report.
        "iw" => "heb",
        "in" => "ind",
        "ji" | "yi" | "yid" => "yid",
        "jw" | "jv" | "jav" => "jav",
        "mo" => "ron",
        other => other,
    }
    .to_string()
}

/// Language value written into the container. Matroska keeps any string, the MP4 muxer needs the
/// ISO 639-2 form; normalising for both keeps existing output unchanged and new MP4 files tagged.
fn tag_language(value: &str, container: Option<&str>) -> String {
    if container == Some("mp4") {
        iso639_2(value)
    } else {
        value.to_owned()
    }
}

fn language_key(value: &str) -> String {
    let mut key = value.to_ascii_lowercase();
    if let Some(end) = key.find(['-', '_']) {
        key.truncate(end);
    }
    // ISO 639-2/T pairs: yt-dlp 2026.08.19 ISO639Utils; bibliographic and
    // deprecated aliases: Unicode CLDR 48 supplementalMetadata.xml.
    // Match the extractor/embedder's complete code set, not only common languages.
    let canonical = match key.as_str() {
        "aar" => "aa",
        "abk" => "ab",
        "ave" => "ae",
        "afr" => "af",
        "aka" => "ak",
        "amh" => "am",
        "arg" => "an",
        "ara" => "ar",
        "asm" => "as",
        "ava" => "av",
        "aym" => "ay",
        "aze" => "az",
        "bak" => "ba",
        "bel" => "be",
        "bul" => "bg",
        "bih" => "bh",
        "bis" => "bi",
        "bam" => "bm",
        "ben" => "bn",
        "bod" | "tib" => "bo",
        "bre" => "br",
        "bos" => "bs",
        "cat" => "ca",
        "che" => "ce",
        "cha" => "ch",
        "cos" => "co",
        "cre" => "cr",
        "ces" | "cze" => "cs",
        "chu" => "cu",
        "chv" => "cv",
        "cym" | "wel" => "cy",
        "dan" => "da",
        "deu" | "ger" => "de",
        "div" => "dv",
        "adp" | "dzo" => "dz",
        "ewe" => "ee",
        "ell" | "gre" => "el",
        "eng" => "en",
        "epo" => "eo",
        "spa" => "es",
        "est" => "et",
        "baq" | "eus" => "eu",
        "fas" | "pe" | "per" => "fa",
        "ful" => "ff",
        "fin" => "fi",
        "fij" => "fj",
        "fao" => "fo",
        "fra" | "fre" => "fr",
        "fry" => "fy",
        "gle" => "ga",
        "gla" => "gd",
        "glg" => "gl",
        "grn" => "gn",
        "guj" => "gu",
        "glv" => "gv",
        "hau" => "ha",
        "heb" | "iw" => "he",
        "hin" => "hi",
        "hmo" => "ho",
        "hrv" | "scr" => "hr",
        "hat" => "ht",
        "hun" => "hu",
        "arm" | "hye" => "hy",
        "her" => "hz",
        "ina" => "ia",
        "in" | "ind" => "id",
        "ile" => "ie",
        "ibo" => "ig",
        "iii" => "ii",
        "ipk" => "ik",
        "ido" => "io",
        "ice" | "isl" => "is",
        "ita" => "it",
        "iku" => "iu",
        "jpn" => "ja",
        "jav" | "jw" => "jv",
        "geo" | "kat" => "ka",
        "kon" => "kg",
        "kik" => "ki",
        "kua" => "kj",
        "kaz" => "kk",
        "kal" => "kl",
        "khm" => "km",
        "kan" => "kn",
        "kor" => "ko",
        "kau" => "kr",
        "kas" => "ks",
        "kur" => "ku",
        "kom" => "kv",
        "cor" => "kw",
        "kir" => "ky",
        "lat" => "la",
        "ltz" => "lb",
        "lug" => "lg",
        "lim" => "li",
        "lin" => "ln",
        "lao" => "lo",
        "lit" => "lt",
        "lub" => "lu",
        "lav" => "lv",
        "mlg" => "mg",
        "mah" => "mh",
        "mao" | "mri" => "mi",
        "mac" | "mkd" => "mk",
        "mal" => "ml",
        "mon" => "mn",
        "mar" => "mr",
        "may" | "msa" => "ms",
        "mlt" => "mt",
        "bur" | "mya" => "my",
        "nau" => "na",
        "nob" => "nb",
        "nde" => "nd",
        "nep" => "ne",
        "ndo" => "ng",
        "dut" | "nld" => "nl",
        "nno" => "nn",
        "nor" => "no",
        "nbl" => "nr",
        "nav" => "nv",
        "nya" => "ny",
        "oci" => "oc",
        "oji" => "oj",
        "orm" => "om",
        "ori" => "or",
        "oss" => "os",
        "pan" => "pa",
        "pli" => "pi",
        "pol" => "pl",
        "pus" => "ps",
        "por" => "pt",
        "que" => "qu",
        "roh" => "rm",
        "run" => "rn",
        "mo" | "ron" | "rum" => "ro",
        "rus" => "ru",
        "kin" => "rw",
        "san" => "sa",
        "srd" => "sc",
        "snd" => "sd",
        "sme" => "se",
        "sag" => "sg",
        "sin" => "si",
        "slk" | "slo" => "sk",
        "slv" => "sl",
        "smo" => "sm",
        "sna" => "sn",
        "som" => "so",
        "alb" | "sqi" => "sq",
        "scc" | "srp" => "sr",
        "ssw" => "ss",
        "sot" => "st",
        "sun" => "su",
        "swe" => "sv",
        "swa" => "sw",
        "tam" => "ta",
        "tel" => "te",
        "tgk" => "tg",
        "tha" => "th",
        "tir" => "ti",
        "tuk" => "tk",
        "tgl" => "tl",
        "tsn" => "tn",
        "ton" => "to",
        "tur" => "tr",
        "tso" => "ts",
        "tat" => "tt",
        "twi" => "tw",
        "tah" => "ty",
        "uig" => "ug",
        "ukr" => "uk",
        "urd" => "ur",
        "uzb" => "uz",
        "ven" => "ve",
        "vie" => "vi",
        "vol" => "vo",
        "wln" => "wa",
        "wol" => "wo",
        "xho" => "xh",
        "ji" | "yid" => "yi",
        "yor" => "yo",
        "zha" => "za",
        "chi" | "zho" => "zh",
        "zul" => "zu",
        _ => return key,
    };
    key.clear();
    key.push_str(canonical);
    key
}

fn validate_track_selection(request: &AddRequest, info: &MediaInfo) -> Result<()> {
    if !request.audio_tracks.is_empty()
        && (request.audio_format_id.is_some() || request.audio_language.is_some())
    {
        bail!(crate::i18n::ui("Çoklu ses seçimi eski tek ses seçimiyle birlikte kullanılamaz", "Multiple audio selection cannot be used together with the legacy single audio selection"));
    }
    if !request.subtitle_tracks.is_empty()
        && (!request.subtitle_languages.is_empty() || request.subtitle_mode.is_some())
    {
        bail!(crate::i18n::ui("Açık altyazı seçimi eski altyazı seçimiyle birlikte kullanılamaz", "Explicit subtitle selection cannot be used together with the legacy subtitle selection"));
    }
    if !request.external_subtitles.is_empty()
        && (!request.subtitle_tracks.is_empty()
            || !request.subtitle_languages.is_empty()
            || request.subtitle_mode.is_some())
    {
        bail!(crate::i18n::ui(
            "HTML track altyazıları ile çıkarıcı altyazıları aynı istekte karıştırılamaz",
            "HTML track subtitles and extractor subtitles cannot be mixed in one request"
        ));
    }
    if let Some(id) = &request.video_format_id {
        let video = info
            .formats
            .iter()
            .find(|format| format.id == *id && !stream_absent(format.video_codec.as_deref()))
            .ok_or_else(|| {
                anyhow!(crate::i18n::ui("Seçilen görüntü biçimi artık bulunmuyor; başka biçime otomatik geçilmedi", "The selected video format is no longer available; no automatic switch to another format was made"))
            })?;
        if request
            .exact_height
            .is_some_and(|height| video.height != Some(height))
        {
            crate::bail_code!(crate::error_codes::MED_012);
        }
        // yt-dlp keeps the first audio stream of a merge, so a dub beside a video that already
        // carries audio would be dropped silently.
        if request.kind != DownloadKind::Audio
            && request.audio_format_id.is_some()
            && stream_present(video.audio_codec.as_deref())
        {
            bail!(crate::i18n::ui(
                "Ayrı ses parçası yalnız ses içermeyen bir görüntü biçimiyle birleştirilebilir",
                "A separate audio track can only be merged with a video format that has no audio"
            ));
        }
    } else if let Some(height) = request.exact_height {
        if !info
            .formats
            .iter()
            .any(|f| f.height == Some(height) && !stream_absent(f.video_codec.as_deref()))
        {
            crate::bail_code!(
                crate::error_codes::MED_012,
                "{}",
                crate::i18n::ui_format!("Seçilen {height}p kalitesi artık bulunmuyor; başka kaliteye otomatik geçilmedi", "The selected {height}p quality is no longer available; no automatic switch to another quality was made", height = height)
            );
        }
    }

    let mut audio_ids = BTreeSet::new();
    for selection in &request.audio_tracks {
        validate_audio_selection(selection)?;
        if !audio_ids.insert(selection.id.as_str()) {
            crate::bail_code!(crate::error_codes::MED_012);
        }
        let audio = exact_audio_format(info, &selection.id)?;
        if let Some(language) = &selection.language {
            if audio.language.as_deref().map(language_key) != Some(language_key(language)) {
                crate::bail_code!(
                    crate::error_codes::MED_012,
                    "{}",
                    crate::i18n::ui_format!(
                        "Seçilen ses parçasının dili değişti: {0}",
                        "The language of the selected audio track changed: {0}",
                        selection.id
                    )
                );
            }
        }
    }
    if let Some(id) = &request.audio_format_id {
        let audio = exact_audio_format(info, id)?;
        if let Some(language) = &request.audio_language {
            if audio.language.as_deref().map(language_key) != Some(language_key(language)) {
                bail!(crate::i18n::ui(
                    "Seçilen ses parçasının dili değişti",
                    "The language of the selected audio track changed"
                ));
            }
        }
    } else if let Some(language) = &request.audio_language {
        if !info.formats.iter().any(|f| {
            (stream_present(f.audio_codec.as_deref())
                || (stream_absent(f.video_codec.as_deref())
                    && !stream_absent(f.audio_codec.as_deref())))
                && f.language.as_deref().map(language_key) == Some(language_key(language))
        }) {
            crate::bail_code!(crate::error_codes::MED_012);
        }
    }

    let mut subtitle_languages = BTreeSet::new();
    for selection in &request.subtitle_tracks {
        validate_subtitle_selection(selection)?;
        if !subtitle_languages.insert(language_key(&selection.language)) {
            bail!(
                "{}",
                crate::i18n::ui_format!("Aynı dil için manuel ve otomatik altyazı birlikte veya yinelenerek seçilemez: {0}", "Manual and automatic subtitles for the same language cannot be selected together or twice: {0}", selection.language)
            );
        }
        if !info.subtitle_tracks.iter().any(|track| {
            track.language == selection.language && track.automatic == selection.automatic
        }) {
            crate::bail_code!(
                crate::error_codes::MED_012,
                "{}",
                crate::i18n::ui_format!(
                    "Seçilen altyazı artık bulunmuyor: {0}",
                    "The selected subtitle is no longer available: {0}",
                    selection.language
                )
            );
        }
    }
    // Legacy CLI subtitle patterns keep yt-dlp semantics; the popup sends exact codes.
    for language in request.subtitle_languages.iter().filter(|language| {
        request.subtitle_mode.is_some()
            || !(language.as_str() == "all" || language.contains(['*', '.']))
    }) {
        let exists = info.subtitle_tracks.iter().any(|s| {
            &s.language == language
                && match request.subtitle_mode.as_deref() {
                    Some("manual") => !s.automatic,
                    Some("automatic") => s.automatic,
                    _ => true,
                }
        });
        if !exists {
            crate::bail_code!(
                crate::error_codes::MED_012,
                "{}",
                crate::i18n::ui_format!(
                    "Seçilen altyazı bulunamadı: {language}",
                    "The selected subtitle was not found: {language}",
                    language = language
                )
            );
        }
    }
    Ok(())
}

fn exact_audio_format<'a>(info: &'a MediaInfo, id: &str) -> Result<&'a MediaFormat> {
    info.formats
        .iter()
        .find(|format| {
            format.id == id
                && stream_absent(format.video_codec.as_deref())
                && !stream_absent(format.audio_codec.as_deref())
        })
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "Seçilen ses parçası artık bulunmuyor: {id}",
                    "The selected audio track is no longer available: {id}",
                    id = id
                )
            )
        })
}

fn validate_audio_selection(selection: &AudioSelection) -> Result<()> {
    validate_format_id(
        &selection.id,
        crate::i18n::ui("Ses format kimliği", "Audio format id"),
    )?;
    if let Some(language) = &selection.language {
        validate_language(language, crate::i18n::ui("Ses dili", "Audio language"))?;
    }
    Ok(())
}

fn validate_subtitle_selection(selection: &SubtitleSelection) -> Result<()> {
    validate_language(
        &selection.language,
        crate::i18n::ui("Altyazı dili", "Subtitle language"),
    )
}

fn selected_tracks_format(request: &AddRequest, info: &MediaInfo) -> Result<String> {
    validate_track_selection(request, info)?;
    let height = request
        .exact_height
        .map(|h| format!("[height={h}]"))
        .or_else(|| request.max_height.map(|h| format!("[height<={h}]")))
        .unwrap_or_default();
    let video = if let Some(id) = &request.video_format_id {
        id.clone()
    } else if request.kind == DownloadKind::Video && request.container.as_deref() == Some("mp4") {
        best_mp4_video(request, info)?.id.clone()
    } else {
        format!("bestvideo{height}")
    };
    let language = request
        .audio_language
        .as_ref()
        .map(|l| format!("[language={l}]"))
        .unwrap_or_default();
    if !request.audio_tracks.is_empty() {
        if request.kind != DownloadKind::Video {
            bail!(crate::i18n::ui(
                "Çoklu ses seçimi yalnız video çıktısında kullanılabilir",
                "Multiple audio selection is only available for video output"
            ));
        }
        if let Some(selected) = request
            .video_format_id
            .as_ref()
            .and_then(|id| info.formats.iter().find(|format| format.id == *id))
        {
            if stream_present(selected.audio_codec.as_deref()) {
                bail!(crate::i18n::ui("Ayrı ses parçaları yalnız ses içermeyen bir görüntü biçimiyle birleştirilebilir", "Separate audio tracks can only be merged with a video format that has no audio"));
            }
        }
        return Ok(std::iter::once(video)
            .chain(request.audio_tracks.iter().map(|track| track.id.clone()))
            .collect::<Vec<_>>()
            .join("+"));
    }
    if let Some(id) = &request.audio_format_id {
        if request.kind == DownloadKind::Audio {
            return Ok(id.clone());
        }
        return Ok(format!("{video}+{id}"));
    }
    if request.kind == DownloadKind::Audio {
        return Ok(format!("bestaudio{language}/best{language}"));
    }
    if request.audio_language.is_some() {
        if request.container.as_deref() == Some("mp4") {
            let audio = best_mp4_audio(request, info)?;
            return Ok(if request.playlist {
                playlist_mp4_selector(request, true)
            } else {
                format!("{video}+{}", audio.id)
            });
        }
        return Ok(format!(
            "{video}+bestaudio{language}/best{height}{language}"
        ));
    }
    if let Some(id) = &request.video_format_id {
        let selected = info
            .formats
            .iter()
            .find(|format| format.id == *id)
            .context(crate::i18n::ui(
                "Seçilen görüntü biçimi bulunamadı",
                "The selected video format was not found",
            ))?;
        if stream_present(selected.audio_codec.as_deref()) || !audio_offered(info) {
            return Ok(id.clone());
        }
        if request.container.as_deref() == Some("mp4") {
            return Ok(format!("{id}+{}", best_mp4_audio(request, info)?.id));
        }
        return Ok(format!("{id}+bestaudio"));
    }
    if !audio_offered(info) {
        return Ok(video);
    }
    if request.container.as_deref() == Some("mp4") {
        return Ok(format!("{video}+{}", best_mp4_audio(request, info)?.id));
    }
    Ok(format!("bestvideo{height}+bestaudio/best{height}"))
}

fn reject_unsafe_media_headers(headers: &std::collections::BTreeMap<String, String>) -> Result<()> {
    if headers.keys().any(|name| {
        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie")
    }) {
        bail!(crate::i18n::ui("Authorization veya Cookie başlığı tek medya başlık bloğuyla ana kaynak, alt liste ve parça sunucularına güvenle sınırlandırılamaz. Kaynak kapsamlı oturum çerezleri veya imzalı bağlantı kullanın.", "An Authorization or Cookie header cannot be safely limited to the main source, sub-playlist and segment servers with a single media header block. Use source-scoped session cookies or a signed link."));
    }
    Ok(())
}

/// An id is embedded in the `--format` expression, so it may not hold a selector operator or
/// bracket. `=` stays allowed: DASH ids such as `video=1500000` carry it and yt-dlp reads it as
/// part of the id.
fn validate_format_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || !value.chars().all(|character| {
            character.is_alphanumeric() || matches!(character, '-' | '_' | '.' | '=')
        })
    {
        bail!(
            "{}",
            crate::i18n::ui_format!("{label} geçersiz", "{label} is invalid", label = label)
        );
    }
    Ok(())
}

fn validate_language(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        bail!(
            "{}",
            crate::i18n::ui_format!("{label} geçersiz", "{label} is invalid", label = label)
        );
    }
    Ok(())
}

fn validate_external_subtitles(values: &[ExternalSubtitle]) -> Result<()> {
    if values.iter().filter(|subtitle| subtitle.is_default).count() > 1 {
        bail!(crate::i18n::ui(
            "Yalnız bir harici altyazı varsayılan olabilir",
            "Only one external subtitle can be the default"
        ));
    }
    let mut identities = BTreeSet::new();
    for subtitle in values {
        validate_web_url(&subtitle.url)?;
        validate_language(
            &subtitle.language,
            crate::i18n::ui("Harici altyazı dili", "External subtitle language"),
        )?;
        if !matches!(subtitle.kind.as_str(), "subtitles" | "captions") {
            bail!(crate::i18n::ui(
                "Harici track türü yalnız subtitles veya captions olabilir",
                "The external track kind can only be subtitles or captions"
            ));
        }
        if subtitle.label.len() > 200
            || subtitle
                .label
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
        {
            bail!(crate::i18n::ui(
                "Harici altyazı etiketi geçersiz",
                "The external subtitle label is invalid"
            ));
        }
        crate::validation::validate_headers(&subtitle.headers)?;
        reject_unsafe_media_headers(&subtitle.headers)?;
        if let Some(referer) = &subtitle.referer {
            validate_web_url(referer)?;
        }
        if !identities.insert((&subtitle.url, &subtitle.language, &subtitle.kind)) {
            bail!(crate::i18n::ui(
                "Aynı harici altyazı birden fazla kez seçilemez",
                "The same external subtitle cannot be selected more than once"
            ));
        }
    }
    Ok(())
}

fn audio_conversion(request: &AddRequest) -> Result<Option<&'static str>> {
    if request.kind != DownloadKind::Audio {
        return Ok(None);
    }
    let value = request
        .audio_format
        .as_deref()
        .unwrap_or("original")
        .trim()
        .to_ascii_lowercase();
    match value.as_str() {
        "" | "original" | "best" => Ok(None),
        "mp3" => Ok(Some("mp3")),
        "m4a" => Ok(Some("m4a")),
        "opus" => Ok(Some("opus")),
        "flac" => Ok(Some("flac")),
        other => bail!(
            "{}",
            crate::i18n::ui_format!("Desteklenmeyen ses çıktısı: {other}. Kullanılabilir seçenekler: original, MP3, M4A, Opus, FLAC", "Unsupported audio output: {other}. Available options: original, MP3, M4A, Opus, FLAC", other = other)
        ),
    }
}

fn validate_subtitle_languages(values: &[String]) -> Result<String> {
    let mut valid = Vec::with_capacity(values.len());
    for value in values {
        let value = value.trim();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'*'))
        {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Geçersiz altyazı dili seçimi: {value}",
                    "Invalid subtitle language selection: {value}",
                    value = value
                )
            );
        }
        valid.push(value);
    }
    Ok(valid.join(","))
}

#[cfg(test)]
mod hls_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod output_validation_tests {
    use super::*;

    #[test]
    fn a_download_takes_the_pickers_analysis_of_the_same_source_once() {
        let picked = InspectRequest {
            url: "https://media.example/master.m3u8".into(),
            referer: Some("https://site.example/".into()),
            ..InspectRequest::default()
        };
        let analysis = json!({"title": "Film", "formats": [{"format_id": "hd", "url": "https://media.example/hd.m3u8"}]});
        let other = InspectRequest {
            referer: None,
            ..picked.clone()
        };

        remember_inspection(&picked, &analysis);
        // A different source neither gets nor discards it.
        assert!(reuse_inspection(&other).is_none());
        let (value, info) = reuse_inspection(&picked).expect("the picker's analysis");
        assert_eq!(value, analysis);
        assert_eq!(info.title, "Film");
        // Taken once: a retry analyzes afresh.
        assert!(reuse_inspection(&picked).is_none());

        remember_inspection(&picked, &analysis);
        if let Some((_, at, _)) = RECENT_INSPECTION.lock().unwrap().as_mut() {
            *at = Instant::now() - INSPECTION_REUSE;
        }
        assert!(reuse_inspection(&picked).is_none());
    }
    use serde_json::json;
    #[test]
    fn output_validation_rejects_missing_audio_and_wrong_duration_or_quality() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            max_height: Some(360),
            ..Default::default()
        };
        let info = MediaInfo {
            duration: Some(10.0),
            ..Default::default()
        };
        let mut probe = json!({"streams":[{"codec_type":"video","codec_name":"h264","height":360}],"format":{"duration":"10"}});
        assert!(validate_probe(&probe, &request, &info).is_err());
        probe["streams"]
            .as_array_mut()
            .unwrap()
            .push(json!({"codec_type":"audio","codec_name":"aac"}));
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["format"]["duration"] = json!("3");
        assert!(validate_probe(&probe, &request, &info).is_err());
        probe["format"]["duration"] = json!("10");
        probe["streams"][0]["height"] = json!(720);
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn explicit_silence_is_distinct_from_unknown_or_previously_expected_audio() {
        let mut request = AddRequest {
            kind: DownloadKind::Video,
            ..Default::default()
        };
        let probe = json!({"streams":[{"codec_type":"video","codec_name":"h264"}],"format":{"duration":"2"}});
        let info = media_info_from_json(
            &json!({"formats":[{"format_id":"v","vcodec":"h264","acodec":"none"}]}),
        )
        .unwrap();
        assert!(validate_probe(&probe, &request, &info).is_ok());
        assert!(validate_probe(&probe, &request, &MediaInfo::default()).is_err());
        request.expected_audio = Some(true);
        assert!(validate_probe(&probe, &request, &info).is_err());
        request.format_id = Some("v".into());
        // No identified audio stream anywhere in the manifest: pairing with
        // bestaudio would make yt-dlp fail before any byte is downloaded.
        assert_eq!(format_selector(&request, &info).unwrap(), "v");
    }

    #[test]
    fn media_request_preserves_full_observed_referer() {
        let referer = "https://example.test/watch/film?id=12";
        let input = SensitiveInputs::create(
            "https://cdn.test/film.m3u8",
            &Default::default(),
            Some(referer),
            None,
            &[],
        )
        .unwrap();
        assert!(std::str::from_utf8(&input.config)
            .unwrap()
            .contains(referer));
    }

    #[test]
    fn partition_cookies_need_the_page_context_and_never_a_referer() {
        let media = "https://cdn.test/master.m3u8";
        let page = "https://watch.example.test/title";
        let consent = ScopedCookie {
            name: "sid".into(),
            value: "private".into(),
            domain: "cdn.test".into(),
            path: "/".into(),
            secure: true,
            host_only: true,
            store_id: Some("profile-1".into()),
            partition_key: Some("https://example.test".into()),
            ..Default::default()
        };
        let authorized = |page_url: Option<&str>, referer: Option<&str>| {
            SensitiveInputs::create(
                media,
                &Default::default(),
                referer,
                page_url,
                std::slice::from_ref(&consent),
            )
            .map(|input| std::str::from_utf8(&input.config).unwrap().to_owned())
        };

        // The browser handed this off from the page and observed no Referer at all: the
        // partitioned cookie still reaches yt-dlp, and the page never becomes a Referer.
        let input = SensitiveInputs::create(
            media,
            &Default::default(),
            None,
            Some(page),
            std::slice::from_ref(&consent),
        )
        .unwrap();
        let config = std::str::from_utf8(&input.config).unwrap();
        assert!(config.contains("--cookies"));
        assert!(!config.contains("--referer"));
        assert!(!config.contains(page));
        let jar = std::fs::read_to_string(input.cookie_jar.as_ref().unwrap()).unwrap();
        assert!(jar.starts_with("# Netscape HTTP Cookie File"));
        assert!(jar.contains("\tsid\tprivate"));

        // No page means no context to authorize the partition: the request is refused
        // instead of sending the cookie unscoped or silently dropping it.
        assert!(authorized(None, None).is_err());
        assert!(authorized(Some(""), None).is_err());
        // An independently observed Referer is not the page the cookie was collected on,
        // even when its host would satisfy the partition key.
        assert!(authorized(None, Some(page)).is_err());
        assert!(authorized(Some("https://elsewhere.test/title"), Some(page)).is_err());
        // A supplied page must be a real web source page before it authorizes anything.
        for invalid in ["not a url", "file:///C:/page.html"] {
            assert!(authorized(Some(invalid), None).is_err());
        }
        // Two different partitions still cannot be combined in one request.
        let mut other_partition = consent.clone();
        other_partition.name = "other".into();
        other_partition.partition_key = Some("https://second.example.test".into());
        assert!(SensitiveInputs::create(
            media,
            &Default::default(),
            None,
            Some(page),
            &[consent.clone(), other_partition],
        )
        .is_err());

        // The matching page authorizes the cookie, while its own network Referer keeps the
        // exactly observed value.
        let referer = "https://embed.other.test/frame";
        let config = authorized(Some(page), Some(referer)).unwrap();
        assert!(config.contains("--cookies"));
        assert!(config.contains(referer));
    }

    #[test]
    fn bounded_source_tolerance_rejects_minute_truncation_but_accepts_segment_drift() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            expected_audio: Some(true),
            ..Default::default()
        };
        let info = MediaInfo {
            duration: Some(6024.290),
            duration_tolerance: Some(6.25),
            ..Default::default()
        };
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","start_time":"0","duration":"6023.0646"},
            {"codec_type":"audio","codec_name":"aac","start_time":"0.021","duration":"6023.00"}
        ],"format":{"start_time":"0","duration":"6023.0646"}});
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["format"]["duration"] = json!("5964.0");
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn whole_second_metadata_does_not_excuse_truncated_media() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            expected_audio: Some(true),
            ..Default::default()
        };
        let mut info = MediaInfo {
            duration: Some(6023.0),
            ..Default::default()
        };
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","start_time":"0.021","duration":"6024.269"},
            {"codec_type":"audio","codec_name":"aac","start_time":"0","duration":"6024.277"}
        ],"format":{"start_time":"0","duration":"6024.290"}});
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["format"]["duration"] = json!("6022.49");
        assert!(validate_probe(&probe, &request, &info).is_err());
        probe["format"]["duration"] = json!("6024.51");
        assert!(validate_probe(&probe, &request, &info).is_err());
        info.duration = Some(6023.0646);
        probe["format"]["duration"] = json!("6024.290");
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn timestamp_units_cover_vfr_end_alignment_without_percentages() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            expected_audio: Some(true),
            ..Default::default()
        };
        let info = MediaInfo {
            duration: Some(12.0),
            duration_tolerance: Some(0.5),
            ..Default::default()
        };
        let probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","time_base":"1/90000","start_pts":0,"duration_ts":1080000},
            {"codec_type":"audio","codec_name":"aac","time_base":"1/48000","start_pts":1024,"duration_ts":575000}
        ],"format":{}});
        assert!(validate_probe(&probe, &request, &info).is_ok());
    }

    #[test]
    fn clean_independent_stream_tail_is_bounded_without_hiding_a_different_cut() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            expected_audio: Some(true),
            ..Default::default()
        };
        let mut info = MediaInfo::default();
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","start_time":"0","duration":"5604.5"},
            {"codec_type":"audio","codec_name":"aac","start_time":"0","duration":"5603.09"}
        ],"format":{"start_time":"0","duration":"5604.5"}});
        assert!(validate_probe(&probe, &request, &info).is_err());
        let mut source = PinnedMediaSource {
            inspection: json!({}),
            manifests: vec![PinnedManifest {
                marker: "selected".into(),
                body: "#EXTM3U\n#EXTINF:1.25,\nsegment.ts\n#EXT-X-ENDLIST\n".into(),
            }],
            dash_formats: Vec::new(),
        };
        info.duration_tolerance = pinned_hls_duration_tolerance(&source);
        assert!(validate_probe(&probe, &request, &info).is_ok());
        source.manifests[0].body = "#EXTM3U\n#EXTINF:NaN,\nsegment.ts\n#EXT-X-ENDLIST\n".into();
        info.duration_tolerance = pinned_hls_duration_tolerance(&source);
        assert!(validate_probe(&probe, &request, &info).is_err());

        info.duration_tolerance = Some(1.5);
        probe["streams"][1]["duration"] = json!(5600.0);
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn declared_lengths_need_every_selected_stream_as_a_finished_pinned_hls_playlist() {
        let finished = "#EXTM3U
#EXTINF:6.0,
a.ts
#EXTINF:4.0,
b.ts
#EXT-X-ENDLIST
";
        let mut source = PinnedMediaSource {
            // A muxed format with unknown codecs counts for both kinds.
            inspection: json!({"formats": [{"format_id": "muxed", "url": "pinned-0"}]}),
            manifests: vec![PinnedManifest {
                marker: "pinned-0".into(),
                body: finished.into(),
            }],
            dash_formats: Vec::new(),
        };
        assert_eq!(
            pinned_hls_declared_durations(&source),
            (vec![10.0], vec![10.0])
        );

        source.dash_formats.push(PinnedDashFormat {
            entry_index: None,
            format_id: "dash-audio".into(),
        });
        assert_eq!(pinned_hls_declared_durations(&source), (vec![], vec![]));
        source.dash_formats.clear();

        source.manifests[0].body = finished.replace(
            "#EXT-X-ENDLIST
",
            "",
        );
        assert_eq!(pinned_hls_declared_durations(&source), (vec![], vec![]));
        source.manifests[0].body = finished.into();

        source.manifests[0].marker = "pinned-1".into();
        assert_eq!(pinned_hls_declared_durations(&source), (vec![], vec![]));
    }

    #[test]
    fn streams_are_held_to_their_own_playlists_when_the_source_declares_different_lengths() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            expected_audio: Some(true),
            ..Default::default()
        };
        let playlist = |length: &str, segments: usize| {
            format!(
                "#EXTM3U
#EXT-X-TARGETDURATION:7
{}#EXT-X-ENDLIST
",
                format!(
                    "#EXTINF:{length},
segment.ts
"
                )
                .repeat(segments)
            )
        };
        let source = |length: &str, video: usize, audio: usize| PinnedMediaSource {
            inspection: json!({"formats": [
                {"format_id": "video", "url": "pinned-0", "vcodec": "avc1.640028", "acodec": "none"},
                {"format_id": "audio", "url": "pinned-1", "vcodec": "none", "acodec": null}
            ]}),
            manifests: vec![
                PinnedManifest {
                    marker: "pinned-0".into(),
                    body: playlist(length, video),
                },
                PinnedManifest {
                    marker: "pinned-1".into(),
                    body: playlist(length, audio),
                },
            ],
            dash_formats: Vec::new(),
        };
        // The audio playlist runs 169 s past the video's, as the source serves it.
        let exact = source("7.04", 999, 1023);
        let mut info = MediaInfo {
            duration: Some(7201.9),
            duration_tolerance: pinned_hls_duration_tolerance(&exact),
            ..Default::default()
        };
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","start_time":"0","duration":"7032.4"},
            {"codec_type":"audio","codec_name":"aac","start_time":"0","duration":"7201.9"}
        ],"format":{"start_time":"0","duration":"7201.9"}});
        assert!(validate_probe(&probe, &request, &info).is_err());

        (info.declared_video_durations, info.declared_audio_durations) =
            pinned_hls_declared_durations(&exact);
        assert!((info.declared_video_durations[0] - 7032.96).abs() < 1e-6);
        assert!((info.declared_audio_durations[0] - 7201.92).abs() < 1e-6);
        assert!(validate_probe(&probe, &request, &info).is_ok());

        // A lost or repeated fragment still fails against the stream's own playlist.
        probe["streams"][1]["duration"] = json!("7194.9");
        let error = validate_probe(&probe, &request, &info)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ses akışı 7194.9 sn, kaynak listesi 7201.9 sn"),
            "{error}"
        );
        probe["streams"][1]["duration"] = json!("7201.9");
        probe["streams"][0]["duration"] = json!("7039.9");
        assert!(validate_probe(&probe, &request, &info).is_err());
        // An audio stream cut to the video's length does not pass as the video's playlist.
        probe["streams"][0]["duration"] = json!("7032.4");
        probe["streams"][1]["duration"] = json!("7033.0");
        assert!(validate_probe(&probe, &request, &info).is_err());

        // Whole-second lengths are rounded: an hour of 6.006 s segments listed as 6 passes; a
        // stream one segment short does not, against its counterpart's drift, nor three short
        // against its own playlist.
        let rounded = source("6", 600, 600);
        (info.declared_video_durations, info.declared_audio_durations) =
            pinned_hls_declared_durations(&rounded);
        info.duration_tolerance = pinned_hls_duration_tolerance(&rounded);
        probe["streams"][0]["duration"] = json!("3603.6");
        probe["streams"][1]["duration"] = json!("3603.6");
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["streams"][1]["duration"] = json!("3597.6");
        assert!(validate_probe(&probe, &request, &info).is_err());
        probe["streams"][1]["duration"] = json!("3585.6");
        probe["streams"][0]["duration"] = json!("3585.6");
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn a_stream_without_a_size_is_estimated_from_its_bitrate_and_the_duration() {
        let info = media_info_from_json(&json!({"duration": 1790.0, "formats":[
            {"format_id":"hls-1924","tbr":1924.0,"vcodec":"avc1","acodec":"mp4a"},
            {"format_id":"exact","filesize":5000,"filesize_approx":9000,"tbr":64.0,"vcodec":"none","acodec":"aac"},
            {"format_id":"approx","filesize_approx":7000,"tbr":64.0,"vcodec":"none","acodec":"aac"},
            {"format_id":"audio","vcodec":"none","acodec":"mp4a"}
        ]}))
        .unwrap();
        let size = |id: &str| {
            let format = info.formats.iter().find(|format| format.id == id).unwrap();
            (format.filesize, format.size_estimated)
        };
        assert_eq!(size("hls-1924"), (Some(430_495_000), true));
        assert_eq!(size("exact"), (Some(5000), false));
        assert_eq!(size("approx"), (Some(7000), true));
        assert_eq!(size("audio"), (None, false));
        let unknown_length = media_info_from_json(&json!({"formats":[
            {"format_id":"hls-1924","tbr":1924.0,"vcodec":"avc1","acodec":"mp4a"}
        ]}))
        .unwrap();
        assert_eq!(unknown_length.formats[0].filesize, None);
    }

    #[test]
    fn external_subtitles_are_verified_as_each_container_stores_them() {
        let subtitle = |language: &str| ExternalSubtitle {
            url: "https://cdn.example/sub.vtt".into(),
            language: language.into(),
            kind: "subtitles".into(),
            ..ExternalSubtitle::default()
        };
        let request = |container: &str, subtitles: Vec<ExternalSubtitle>| AddRequest {
            kind: DownloadKind::Video,
            container: Some(container.into()).filter(|value: &String| !value.is_empty()),
            external_subtitles: subtitles,
            ..Default::default()
        };
        let probe_format = |format: &str, subtitles: &[(Option<&str>, i64)]| {
            let mut streams = vec![
                json!({"codec_type":"video","codec_name":"h264","start_time":"0","duration":"60"}),
                json!({"codec_type":"audio","codec_name":"aac","start_time":"0","duration":"60"}),
            ];
            for (language, default) in subtitles {
                let mut stream = json!({"codec_type":"subtitle","codec_name":"mov_text","disposition":{"default": default}});
                if let Some(language) = language {
                    stream["tags"] = json!({"language": language});
                }
                streams.push(stream);
            }
            json!({"streams": streams, "format": {"format_name": format, "start_time":"0","duration":"60"}})
        };
        let probe =
            |subtitles: &[(Option<&str>, i64)]| probe_format("mov,mp4,m4a,3gp,3g2,mj2", subtitles);
        let probe_mkv =
            |subtitles: &[(Option<&str>, i64)]| probe_format("matroska,webm", subtitles);
        let info = MediaInfo::default();
        // ffmpeg's MP4 muxer enables the first subtitle when none is default; it reads back
        // as default (checked with the bundled ffmpeg/ffprobe).
        let mp4 = request("mp4", vec![subtitle("tr"), subtitle("en")]);
        assert!(validate_probe(&probe(&[(Some("tur"), 1), (Some("eng"), 0)]), &mp4, &info).is_ok());
        assert!(
            validate_probe(&probe(&[(Some("tur"), 0), (Some("eng"), 0)]), &mp4, &info).is_err()
        );
        let chosen = request(
            "mp4",
            vec![
                subtitle("tr"),
                ExternalSubtitle {
                    is_default: true,
                    ..subtitle("en")
                },
            ],
        );
        assert!(validate_probe(
            &probe(&[(Some("tur"), 0), (Some("eng"), 1)]),
            &chosen,
            &info
        )
        .is_ok());
        assert!(validate_probe(
            &probe(&[(Some("tur"), 1), (Some("eng"), 1)]),
            &chosen,
            &info
        )
        .is_err());
        // Without a container the source's MP4 is kept, and it enables its first subtitle too.
        let kept = request("", vec![subtitle("tr"), subtitle("en")]);
        assert!(
            validate_probe(&probe(&[(Some("tur"), 1), (Some("eng"), 0)]), &kept, &info).is_ok()
        );
        assert!(
            validate_probe(&probe(&[(Some("tur"), 0), (Some("eng"), 0)]), &kept, &info).is_err()
        );
        // Matroska writes no tag for an undetermined language, and keeps the flag as set.
        let mkv = request("mkv", vec![subtitle("und")]);
        assert!(validate_probe(&probe_mkv(&[(None, 0)]), &mkv, &info).is_ok());
        assert!(validate_probe(&probe_mkv(&[(None, 1)]), &mkv, &info).is_err());
        let turkish = request("mkv", vec![subtitle("tr")]);
        assert!(validate_probe(&probe_mkv(&[(None, 0)]), &turkish, &info).is_err());
        assert!(validate_probe(&probe_mkv(&[(Some("tur"), 0)]), &turkish, &info).is_ok());
    }

    #[test]
    fn subtitle_cues_ffmpeg_cannot_store_are_dropped_or_joined() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
                   2\n00:00:03,50 --> 00:00:03,50\nInvisible\n\n\
                   3\n00:00:04,000 --> 00:00:05,000\nWhere\n\n\
                   4\n00:00:04,000 --> 00:00:06,000\nwhere.\n\n\
                   5\n00:00:04,000 --> 00:00:05,000\nWhere\n\n\
                   6\n00:00:05,500 --> 00:00:07,000\nOverlap\n";
        assert_eq!(
            without_unmuxable_cues(srt).unwrap(),
            "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
             3\n00:00:04,000 --> 00:00:06,000\nWhere\nwhere.\n\n\
             6\n00:00:05,500 --> 00:00:07,000\nOverlap\n"
        );
        let vtt =
            "\u{feff}WEBVTT\r\n\r\nintro\r\n00:00:01.000 --> 00:00:02.000 align:start\r\nA\r\n\r\n\
                   00:00:01.000 --> 00:00:03.000 line:0\r\nB\r\n";
        assert_eq!(
            without_unmuxable_cues(vtt).unwrap(),
            "\u{feff}WEBVTT\n\nintro\n00:00:01.000 --> 00:00:03.000 align:start\nA\nB\n"
        );
        let clean =
            "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nA\n\n00:00:02.000 --> 00:00:04.000\nB\n";
        assert_eq!(without_unmuxable_cues(clean), None);
    }

    #[test]
    fn cue_rewrites_read_lines_and_times_as_ffmpeg_does() {
        // A line of spaces stays inside its cue instead of becoming a cue break.
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:01.000\nGone\n\n\
                   00:00:02.000 --> 00:00:03.000\nA\n \nB\n";
        assert_eq!(
            without_unmuxable_cues(vtt).unwrap(),
            "WEBVTT\n\n00:00:02.000 --> 00:00:03.000\nA\n \nB\n"
        );
        // FFmpeg reads `,5` as 5 ms, so both cues start at 2.005 s and become one.
        let srt = "1\n00:00:02,5 --> 00:00:03,000\nA\n\n2\n00:00:02,005 --> 00:00:04,000\nB\n";
        assert_eq!(
            without_unmuxable_cues(srt).unwrap(),
            "1\n00:00:02,5 --> 00:00:04,000\nA\nB\n"
        );
    }

    #[test]
    fn multiple_audio_ids_languages_and_defaults_are_all_exact() {
        let info = media_info_from_json(&json!({"formats":[
            {"format_id":"v","height":720,"vcodec":"h264","acodec":"none"},
            {"format_id":"en-a","vcodec":"none","acodec":"aac","language":"en"},
            {"format_id":"tr-a","vcodec":"none","acodec":"aac","language":"tr"}
        ]}))
        .unwrap();
        let request = AddRequest {
            kind: DownloadKind::Video,
            video_format_id: Some("v".into()),
            audio_tracks: vec![
                AudioSelection {
                    id: "en-a".into(),
                    language: Some("en".into()),
                },
                AudioSelection {
                    id: "tr-a".into(),
                    language: Some("tr".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(format_selector(&request, &info).unwrap(), "v+en-a+tr-a");
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264","height":720},
            {"codec_type":"audio","codec_name":"aac","tags":{"language":"eng"},"disposition":{"default":1}},
            {"codec_type":"audio","codec_name":"aac","tags":{"language":"tur"},"disposition":{"default":0}}
        ],"format":{"duration":"2"}});
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["streams"][2]["disposition"]["default"] = json!(1);
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn dynamic_range_is_real_metadata_and_mp4_rejects_incompatible_codecs() {
        let info = media_info_from_json(&json!({"formats":[
            {"format_id":"hdr","format_note":"2160p HDR10","height":2160,"fps":24.0,"ext":"webm","vcodec":"vp9.2","acodec":"none"},
            {"format_id":"opus","vcodec":"none","acodec":"opus"}
        ]})).unwrap();
        assert_eq!(info.formats[0].dynamic_range.as_deref(), Some("HDR10"));
        assert!(info.formats[0].label.contains("HDR10"));
        let request = AddRequest {
            url: "https://example.test/master.mpd".into(),
            kind: DownloadKind::Video,
            container: Some("mp4".into()),
            video_format_id: Some("hdr".into()),
            audio_format_id: Some("opus".into()),
            ..Default::default()
        };
        assert!(validate_container_selection(&request, &info).is_err());
    }

    #[test]
    fn non_embeddable_data_is_not_offered_as_subtitles() {
        let info = media_info_from_json(&json!({
            "formats":[{"format_id":"v","vcodec":"h264","acodec":"none"}],
            "subtitles":{
                "live_chat":[{"ext":"json","protocol":"youtube_live_chat_replay"}],
                "tr":[{"ext":"json3"},{"ext":"vtt"}]
            },
            "automatic_captions":{"en":[{"ext":"vtt"}]}
        }))
        .unwrap();
        assert_eq!(info.subtitles, ["en", "tr"]);
        assert_eq!(
            info.subtitle_tracks
                .iter()
                .map(|track| (track.language.as_str(), track.automatic))
                .collect::<Vec<_>>(),
            [("tr", false), ("en", true)]
        );
        let request = AddRequest {
            kind: DownloadKind::Video,
            subtitle_tracks: vec![SubtitleSelection {
                language: "live_chat".into(),
                automatic: false,
            }],
            ..Default::default()
        };
        assert!(validate_track_selection(&request, &info).is_err());
    }

    #[test]
    fn subtitle_verification_accepts_iso_language_equivalence_not_other_languages() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            subtitle_tracks: vec![SubtitleSelection {
                language: "aa".into(),
                automatic: true,
            }],
            ..Default::default()
        };
        let info = MediaInfo {
            duration: Some(5028.0),
            ..Default::default()
        };
        let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"h264"},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"subtitle","codec_name":"mov_text","tags":{"language":"aar"}}
        ],"format":{"duration":"5028.257959"}});
        assert!(validate_probe(&probe, &request, &info).is_ok());
        probe["streams"][2]["tags"]["language"] = json!("abk");
        assert!(validate_probe(&probe, &request, &info).is_err());
        probe["streams"][2]["tags"]["language"] = Value::Null;
        assert!(validate_probe(&probe, &request, &info).is_err());
    }

    #[test]
    fn exact_subtitle_modes_do_not_fall_back_or_collide() {
        let info = media_info_from_json(&json!({
            "formats":[
                {"format_id":"v","height":720,"vcodec":"h264","acodec":"none"},
                {"format_id":"a","vcodec":"none","acodec":"aac","language":"tr"}
            ],
            "subtitles":{"tr":[{"ext":"vtt"}]},
            "automatic_captions":{"tr":[{"ext":"vtt"}],"en":[{"ext":"vtt"}]}
        }))
        .unwrap();
        let mut request = AddRequest {
            kind: DownloadKind::Video,
            subtitle_tracks: vec![SubtitleSelection {
                language: "tr".into(),
                automatic: false,
            }],
            ..Default::default()
        };
        assert!(validate_track_selection(&request, &info).is_ok());
        request.subtitle_tracks.push(SubtitleSelection {
            language: "en".into(),
            automatic: true,
        });
        assert!(validate_track_selection(&request, &info).is_ok());
        request.subtitle_tracks[1].language = "tr".into();
        assert!(validate_track_selection(&request, &info).is_err());
    }

    #[test]
    fn scoped_cookies_keep_store_partition_and_netscape_identity_boundaries() {
        let page = "https://watch.example.test/title";
        let base = ScopedCookie {
            name: "sid".into(),
            value: "private".into(),
            domain: "captions.cdn.test".into(),
            path: "/subtitles/".into(),
            secure: true,
            host_only: true,
            store_id: Some("profile-1".into()),
            partition_key: Some("https://example.test".into()),
            ..Default::default()
        };
        assert!(validate_scoped_cookies(std::slice::from_ref(&base), Some(page)).is_ok());

        let mut other_store = base.clone();
        other_store.name = "other".into();
        other_store.store_id = Some("profile-2".into());
        assert!(validate_scoped_cookies(&[base.clone(), other_store], Some(page)).is_err());

        let mut unpartitioned = base.clone();
        unpartitioned.value = "ordinary".into();
        unpartitioned.partition_key = None;
        assert!(validate_scoped_cookies(&[base, unpartitioned], Some(page)).is_err());
    }

    #[test]
    fn fingerprints_separate_access_refresh_from_track_identity() {
        let mut request = AddRequest {
            url: "https://cdn.test/master.m3u8?signature=one".into(),
            kind: DownloadKind::Video,
            container: Some("mkv".into()),
            video_format_id: Some("v720".into()),
            audio_tracks: vec![AudioSelection {
                id: "tr-a".into(),
                language: Some("tr".into()),
            }],
            expected_audio: Some(true),
            request_id: Some("job-1".into()),
            ..Default::default()
        };
        let selection = selection_fingerprint(&request).unwrap();
        let complete = request_fingerprint(&request).unwrap();
        request
            .headers
            .insert("Referer".into(), "https://page.test/new".into());
        request.referer = Some("https://page.test/new".into());
        assert_eq!(selection_fingerprint(&request).unwrap(), selection);
        assert_eq!(request_fingerprint(&request).unwrap(), complete);
        request.url = "https://cdn.test/master.m3u8?signature=two".into();
        assert_eq!(selection_fingerprint(&request).unwrap(), selection);
        assert_ne!(request_fingerprint(&request).unwrap(), complete);
        request.audio_tracks[0].id = "en-a".into();
        assert_ne!(selection_fingerprint(&request).unwrap(), selection);
    }

    #[test]
    fn raw_credentials_cannot_enter_media_configuration() {
        for name in ["Authorization", "Cookie"] {
            let headers = [(name.into(), "private".into())].into();
            assert!(SensitiveInputs::create(
                "https://media.example.test/show/master.m3u8",
                &headers,
                Some("https://media.example.test/watch"),
                None,
                &[],
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod extractor_error_tests {
    use super::{
        available_qualities, drm_protected, format_unavailable, fragment_loss, iso639_2,
        tag_language, useful_error,
    };

    #[test]
    fn a_fully_drm_protected_video_is_recognized_from_the_extractor_error() {
        // yt-dlp 2026.08.19 raise_no_formats(): no JSON is produced for an all-DRM video.
        assert!(drm_protected(
            "[youtube] Extracting URL: https://example.test/watch\nERROR: [example] abc123: This video is DRM protected"
        ));
        // A warning about skipped formats accompanies videos that still download.
        assert!(!drm_protected(
            "WARNING: [youtube] abc: Some tv client https formats have been skipped as they are DRM protected.\nERROR: [youtube] abc: Sign in to confirm you're not a bot"
        ));
        assert!(!drm_protected(
            "ERROR: Unsupported URL: https://example.test/"
        ));
    }

    #[test]
    fn a_lost_fragment_is_recognized_and_a_clean_run_is_not() {
        assert!(fragment_loss(
            "[download] fragment not found; Skipping fragment 5 ...\n[download] Got error: HTTP Error 429"
        ));
        assert!(fragment_loss("[download] Skipping fragment 12"));
        // With --abort-on-unavailable-fragments the stream is given up instead of skipped.
        assert!(fragment_loss(
            "ERROR: fragment 6 not found, unable to continue
  File \"__main__.py\", line 17"
        ));
        assert!(!fragment_loss(
            "[hlsnative] Total fragments: 10
[download] Destination: media.f1.mp4"
        ));
        // The observed field failure: the fragment temp file vanished under the downloader.
        assert!(fragment_loss(
            "FileNotFoundError: [Errno 2] No such file or directory: '\\\\?\\C:\\Downloads\\SSDownload\\\
             .ssdownload-work\\jobs\\3df08e7f\\media.f2415.mp4.part-Frag146'"
        ));
        assert!(!fragment_loss(
            "[download] 100% of 1.02GiB at 22.8MiB/s ETA 00:00\n[download] Destination: media.mp4"
        ));
    }

    #[test]
    fn a_quiet_transfer_is_recognized_as_stalled_and_activity_clears_it() {
        let mut watch = super::ActivityWatch::new(std::time::Duration::from_millis(50), Vec::new());
        assert!(!watch.stalled());
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(watch.stalled());
        watch.touch();
        assert!(!watch.stalled());
    }

    #[test]
    fn a_silent_postprocessor_is_not_a_stalled_transfer() {
        // FFmpeg merging or converting a long file prints nothing and opens no connection; only
        // the post-processing limit applies between yt-dlp's started and finished markers.
        let mut watch = super::ActivityWatch::new(std::time::Duration::from_millis(20), Vec::new());
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(watch.stalled());
        watch.observe("SSDOWNLOAD_POSTPROCESS\tstarted");
        assert!(!watch.stalled());
        watch.observe("SSDOWNLOAD_POSTPROCESS\tfinished");
        assert!(watch.stalled());
    }

    #[test]
    fn a_fresh_liveness_ping_overrides_stale_progress() {
        let ping = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            crate::network::now_epoch_ms(),
        ));
        let watch =
            super::ActivityWatch::new(std::time::Duration::from_millis(50), vec![ping.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(80));
        // Progress is stale, but a just-seen outbound attempt keeps the transfer alive.
        ping.store(
            crate::network::now_epoch_ms(),
            std::sync::atomic::Ordering::Release,
        );
        assert!(!watch.stalled());
        ping.store(0, std::sync::atomic::Ordering::Release);
        assert!(watch.stalled());
    }

    #[test]
    fn language_tags_are_iso_639_2_for_mp4_and_unchanged_for_matroska() {
        // The MP4 muxer drops anything that is not a three-letter code, so "tr" never reached the
        // file; Matroska keeps the original value and must not be rewritten.
        assert_eq!(tag_language("tr", Some("mp4")), "tur");
        assert_eq!(tag_language("tur", Some("mp4")), "tur");
        assert_eq!(tag_language("tr-TR", Some("mp4")), "tur");
        assert_eq!(tag_language("en", Some("mp4")), "eng");
        assert_eq!(tag_language("tr", Some("mkv")), "tr");
        assert_eq!(tag_language("tr", None), "tr");
        assert_eq!(iso639_2("xyz"), "xyz");
    }

    #[test]
    fn mp4_language_tags_survive_the_output_check_for_dubbed_audio_languages() {
        // A two-letter tag never reaches an MP4 file, so the selected language would read back
        // as undetermined and fail output validation after a complete download.
        for code in [
            "ta", "bn-IN", "te", "ml", "mr", "gu", "kn", "pa", "ur", "ca", "af", "sw", "ms", "iw",
            "in", "ji", "jw", "nb", "sq", "eu", "is", "mk",
        ] {
            let tag = tag_language(code, Some("mp4"));
            assert_eq!(tag.len(), 3, "{code} -> {tag}");
            assert_eq!(
                super::language_key(&tag),
                super::language_key(code),
                "{code} -> {tag}"
            );
        }
        assert_eq!(tag_language("ta", Some("mp4")), "tam");
        assert_eq!(tag_language("iw", Some("mp4")), "heb");
    }

    #[test]
    fn a_missing_requested_format_is_recognized_and_lists_what_remains() {
        // The wording yt-dlp used for the reported 800p selection failure.
        assert!(format_unavailable(
            "ERROR: [generic] master.m3u8?t=abc: Requested format is not available. Use --list-formats"
        ));
        assert!(!format_unavailable("ERROR: HTTP Error 403: Forbidden"));
        assert!(!format_unavailable("yt-dlp: name or service not known"));

        let info = media_info_with_heights(&[480, 1080, 480, 0]);
        let list = available_qualities(&info);
        assert!(list.contains("480p") && list.contains("1080p"), "{list}");
        assert_eq!(
            list.matches("480p").count(),
            1,
            "duplicates must collapse: {list}"
        );
        assert!(
            available_qualities(&media_info_with_heights(&[])).contains("çözünürlük bilgisi yok")
        );
    }

    fn media_info_with_heights(heights: &[u32]) -> crate::model::MediaInfo {
        let formats = heights
            .iter()
            .enumerate()
            .map(|(index, height)| crate::model::MediaFormat {
                id: format!("f{index}"),
                height: (*height > 0).then_some(*height),
                ..Default::default()
            })
            .collect();
        crate::model::MediaInfo {
            formats,
            ..Default::default()
        }
    }

    #[test]
    fn generic_error_hides_query_derived_ids_without_losing_the_failure() {
        let error = useful_error("ERROR: [generic] ?expires=123&sig=private&srcIp=192.0.2.1: Unable to download webpage: HTTP Error 400: Bad Request", &[]);
        assert!(error.contains("HTTP Error 400: Bad Request"));
        assert!(!error.contains("private"));
        assert!(!error.contains("192.0.2.1"));
        let opaque = useful_error(
            "ERROR: [generic] opaque-signed-path: HTTP Error 404: Not Found",
            &[],
        );
        assert_eq!(opaque, "ERROR: [generic] HTTP Error 404: Not Found");
        assert_eq!(
            useful_error("ERROR: [generic] HTTP Error 404: Not Found", &[]),
            "ERROR: [generic] HTTP Error 404: Not Found"
        );
        assert_eq!(
            useful_error("ERROR: connection failed", &[]),
            "ERROR: connection failed"
        );
    }
}
