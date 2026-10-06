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
use selection::*;
pub(crate) use session::sweep_stale_cookie_jars;
use session::*;
use workspace::*;
pub(crate) use workspace::{
    cancel_source_refresh, finish_source_refresh, has_partial_output, prepare_source_refresh,
    stage_source_refresh, PreparedSourceRefresh,
};

const INSPECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_INSPECT_OUTPUT: usize = 128 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 32 * 1024;
/// A downloaded stream must show progress within this window or its transfer is reconnected.
const STALL_LIMIT: Duration = Duration::from_secs(180);
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
        bail!("Kaynak hazırlığı durduruldu");
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
    Ok(inspect_document_with_tools(request, toolchain, control, network)?.1)
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
                bail!("Kaynak hazırlığı durduruldu");
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
        if is_forbidden(&detail) {
            crate::logging::record(crate::logging::Event::failure(
                "inspect.blocked",
                crate::error_codes::MED_007,
                detail.clone(),
            ));
            crate::bail_code!(crate::error_codes::MED_007, "{detail}");
        }
        crate::logging::record(crate::logging::Event::failure(
            "inspect.failed",
            crate::error_codes::MED_006,
            detail.clone(),
        ));
        crate::bail_code!(
            crate::error_codes::MED_006,
            "Medya analizi başarısız: {detail}"
        );
    }
    if captured.stdout.len() > MAX_INSPECT_OUTPUT {
        crate::bail_code!(crate::error_codes::MED_011);
    }
    let value: Value = serde_json::from_slice(&captured.stdout)
        .context("yt-dlp geçerli JSON medya bilgisi döndürmedi")?;
    let info = media_info_from_json(&value)?;
    validate_single_selection(&info, request.playlist)?;
    Ok((value, info))
}

fn validate_single_selection(info: &MediaInfo, playlist: bool) -> Result<()> {
    if !playlist && info.playlist_count.is_some_and(|count| count > 1) {
        crate::bail_code!(crate::error_codes::MED_010, "Sayfa birden fazla bağımsız video içeriyor; fragman veya reklam seçilmemesi için videoyu tarayıcıda oynatıp video üzerindeki düğmeyi kullanın");
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

    let effective_referer = request.referer.as_deref();
    let toolchain = tools::verified_toolchain(paths, control)?;
    let (mut info, source_capture, selector, failure_stages) =
        if let Some((pinned, evidence)) = load_pinned_source_for_request(output, request)? {
            let info = media_info_from_json(&pinned.inspection)?;
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
            )
        } else {
            let inspect_request = InspectRequest {
                url: request.url.clone(),
                headers: request.headers.clone(),
                referer: effective_referer.map(str::to_string),
                page_url: request.page_url.clone(),
                session_cookies: request.session_cookies.clone(),
                playlist: request.playlist,
                ..InspectRequest::default()
            };
            let (inspection, info) =
                inspect_document_with_tools(&inspect_request, &toolchain, control, network)
                    .map_err(|error| {
                        let phase = if media_manifest_url(&request.url) {
                            "ana liste"
                        } else {
                            "keşif"
                        };
                        anyhow!("{phase} aşamasında {error:#}")
                    })?;
            let selector = format_selector(request, &info)?;
            let failure_stages =
                DownloadFailureStages::from_inspection(&inspection, request, &selector);
            let capture =
                capture_media_source(request, &inspection, &info, &selector, control, network)?;
            (info, capture, selector, failure_stages)
        };
    if info.duration_tolerance.is_none() && !request.playlist {
        info.duration_tolerance = source_capture
            .pinned
            .as_ref()
            .and_then(pinned_hls_duration_tolerance);
    }
    validate_track_selection(request, &info)?;
    validate_container_selection(request, &info)?;
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
    if let Some(source_info) = &source_info {
        base_args.extend([
            "--enable-file-urls".into(),
            "--load-info-json".into(),
            source_info.info_path.to_string_lossy().into_owned(),
        ]);
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
    // Fragment parallelism is appended per attempt: a retry after a lost fragment runs with a
    // single connection, because the proxy tunnel limit is what the sources overwhelmed.
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

    let mut completed_files = HashSet::new();
    let mut last_path = None;
    let mut restart_count = 0u32;
    let mut compatibility_retry = false;
    let mut fragment_restarts = 0u32;
    let mut stall_restarts = 0u32;
    loop {
        if control.is_cancelled() {
            bail!("İndirme iptal edildi");
        }
        if control.is_paused() {
            bail!("İndirme duraklatıldı");
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
            if fragment_restarts > 0 {
                "1".to_string()
            } else {
                fragment_concurrency(request.connections, network.per_host_limit())
            },
        ]);
        progress(TransferProgress {
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

        // Files finished before this attempt are known good; everything the attempt itself
        // produces is provisional until it survives a fragment-complete download.
        let known_good = completed_files.clone();
        let mut running = RunningDownload::spawn_ytdlp(
            yt_dlp,
            &args,
            &mut secrets.config,
            &mut secrets.redactions,
            control,
            network,
        )?;
        let outcome = monitor_download(
            &mut running,
            control,
            launch_limit,
            progress,
            &mut completed_files,
            &mut last_path,
        )?;
        match outcome {
            MonitorOutcome::RestartForSpeed => {
                restart_count = restart_count.saturating_add(1);
                discard_fragmented_restart(request, &output_plan, &known_good, job_id)?;
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
                discard_fragmented_restart(request, &output_plan, &known_good, job_id)?;
                progress(TransferProgress {
                    phase: crate::i18n::ui_owned!(
                        format!(
                            "Aktarım ilerlemedi; bağlantı yenileniyor ({}).",
                            stall_restarts
                        ),
                        format!("The transfer stalled; reconnecting ({}).", stall_restarts)
                    ),
                    ..TransferProgress::default()
                });
                continue;
            }
            MonitorOutcome::Paused => crate::bail_code!(crate::error_codes::TRF_009),
            MonitorOutcome::Cancelled => crate::bail_code!(crate::error_codes::TRF_008),
            MonitorOutcome::Exited(status) => {
                if status.success() {
                    if let Some(error) = running.input_error.take() {
                        return Err(error).context("Medya isteği gönderilemedi");
                    }
                }
                // A skipped fragment leaves a permanent hole: discard this attempt's own files so
                // the next one downloads the affected stream again instead of resuming it.
                if fragment_loss(&running.stderr_text()) {
                    let discarded = discard_attempt_outputs(&output_plan, &known_good)?;
                    if discarded.0 > 0 {
                        fragment_restarts = fragment_restarts.saturating_add(1);
                        progress(TransferProgress {
                            phase: crate::i18n::ui_owned!(
                                format!(
                                    "Parça atlandı; {} dosya ({:.1} MB) atılıp akış baştan indiriliyor (deneme {}, tek bağlantı).",
                                    discarded.0,
                                    discarded.1 as f64 / (1024.0 * 1024.0),
                                    fragment_restarts + 1
                                ),
                                format!(
                                    "A fragment was skipped; {} file(s) ({:.1} MB) discarded and the stream restarts (attempt {}, single connection).",
                                    discarded.0,
                                    discarded.1 as f64 / (1024.0 * 1024.0),
                                    fragment_restarts + 1
                                )
                            ),
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
                                "Kaynak parçaları tekrar tekrar düşürdü; akış {} kez baştan indirildi ve bütünlük sağlanamadı. Kaynağı daha sonra yeniden deneyin.",
                                fragment_restarts
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
                        progress(TransferProgress { phase:"Sunucu 403 döndürdü; aynı kaynak tarayıcı uyumlu bağlantıyla bir kez deneniyor".into(), ..TransferProgress::default() });
                        continue;
                    }
                    let redacted = useful_error(&error, &secrets.redactions);
                    if format_unavailable(&redacted) {
                        crate::bail_code!(
                            crate::error_codes::MED_015,
                            "Seçilen kalite indirme anında kaynağın listesinde yok; kaynak yenilenmiş olabilir. Kaynağı yeniden çözümleyip var olan bir kaliteyi seçin. {}",
                            available_qualities(&info)
                        );
                    }
                    bail!(
                        "{} aşamasında medya indirmesi başarısız (uyumluluk denemesi: {}/1): {}",
                        classify_download_error(&redacted, request).0,
                        u8::from(compatibility_retry),
                        redacted
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
        downloaded: bytes,
        total: Some(bytes),
        speed: 0,
        eta: Some(0),
        phase: crate::i18n::ui("Tamamlandı", "Completed").into(),
    });
    Ok(TransferResult {
        path: result_path,
        bytes,
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
        bail!("Doğrulanacak tarayıcı medya çıktısı bulunamadı");
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
        bail!("Tarayıcı medya hedefi zaten var; mevcut kullanıcı dosyasının üzerine yazılmadı");
    }
    for stream in streams {
        if !stream.path.is_file() {
            bail!("Tarayıcı medya akışı çalışma dosyasında bulunamadı");
        }
        if !matches!(stream.role.as_str(), "video" | "audio" | "subtitle") {
            bail!("Tarayıcı bilinmeyen bir medya akışı rolü gönderdi");
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
        bail!("Tarayıcı aktarımında tam bir seçili görüntü akışı bulunamadı");
    }
    if request.kind == DownloadKind::Audio && !videos.is_empty() {
        bail!("Tarayıcı yalnız ses aktarımında görüntü akışı gönderdi");
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
        bail!("Tarayıcı aktarımında seçilen ses akışlarının tamamı bulunamadı");
    }
    for (index, stream) in audios.iter().enumerate() {
        let expected_id = request
            .audio_tracks
            .get(index)
            .map(|selection| selection.id.as_str())
            .or(request.audio_format_id.as_deref());
        if expected_id.is_some() && stream.format_id.as_deref() != expected_id {
            bail!("Tarayıcı ses akışı seçilen format kimliğiyle eşleşmiyor");
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
            bail!("Tarayıcı ses akışı seçilen dille eşleşmiyor");
        }
    }
    if let Some(id) = &request.video_format_id {
        let video = videos
            .first()
            .context("Tarayıcı aktarımında seçilen görüntü akışı bulunamadı")?;
        if video.format_id.as_deref() != Some(id.as_str()) {
            bail!("Tarayıcı görüntü akışı seçilen format kimliğiyle eşleşmiyor");
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
        bail!("Tarayıcı aktarımında seçilen altyazı akışlarının tamamı bulunamadı");
    }

    let tools = tools::verified_toolchain(paths, control)?;
    let parent = output
        .parent()
        .context("Tarayıcı medya hedef klasörü eksik")?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".browser-mux-{}.tmp", uuid::Uuid::new_v4()));
    let mut args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-nostdin".into(),
        "-protocol_whitelist".into(),
        "file,crypto,data".into(),
    ];
    for stream in streams {
        args.extend(["-i".into(), stream.path.to_string_lossy().into_owned()]);
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
    if !subtitles.is_empty() {
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
    for index in 0..subtitles.len() {
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
            "Tarayıcı medya akışları kayıpsız birleştirilemedi: {}",
            bounded_tail(&result.stderr, 4096)
        );
    }
    if let Err(error) = fs::rename(&staged, output) {
        let _ = fs::remove_file(&staged);
        return Err(error).context("Tarayıcı medya çıktısı hedefe alınamadı");
    }
    Ok(())
}

fn common_args(deno: &Path, ffmpeg: &Path, playlist: bool) -> Vec<String> {
    let ffmpeg_dir = ffmpeg.parent().unwrap_or(ffmpeg);
    vec![
        "--ignore-config".into(),
        "--no-plugin-dirs".into(),
        "--no-exec".into(),
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

/// yt-dlp reports a hole in the stream instead of failing the download: "[download] fragment not
/// found; Skipping fragment 5". A resumed `.part` file keeps that hole forever, so the attempt
/// that produced it must be thrown away rather than continued.
/// A fragmented (HLS/DASH) run killed mid-chain must not resume its fragment state:
/// observed yt-dlp resume states can treat a never-fetched fragment as done. Discard
/// this attempt's own outputs back to the known-good set; completed playlist entries
/// (`known_good`) are kept, so multi-entry playlists lose only the unfinished entry.
fn discard_fragmented_restart(
    request: &AddRequest,
    output_plan: &OutputPlan,
    known_good: &HashSet<PathBuf>,
    job_id: &str,
) -> Result<()> {
    let lowered = request.url.to_ascii_lowercase();
    let path = lowered.split('?').next().unwrap_or(&lowered);
    if !(path.ends_with(".m3u8") || path.ends_with(".mpd")) {
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
        || stderr.contains("fragment not found")
        || stderr.contains("Skipping already downloaded fragment")
    {
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
        return "Analizde çözünürlük bilgisi yok.".into();
    }
    let list = heights
        .iter()
        .map(|height| format!("{height}p"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Analizde görülen çözünürlükler: {list}")
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
fn fragment_concurrency(requested: Option<u8>, per_host_limit: u8) -> String {
    // One tunnel of the per-host bound stays free. A slow fragment keeps its connection open while
    // the downloader opens a fresh one for a retry; a job that holds the whole bound turns that
    // retry into a local 503 and then into a lost fragment.
    let headroom = per_host_limit.saturating_sub(1).max(1);
    requested
        .unwrap_or(4)
        .clamp(1, 16)
        .min(headroom)
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

pub(crate) fn classify_download_error(
    error: &str,
    request: &AddRequest,
) -> (&'static str, &'static str) {
    let lower = error.to_ascii_lowercase();
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
        || ((!request.audio_tracks.is_empty()
            || request.audio_format_id.is_some()
            || request.expected_audio == Some(true))
            && lower.contains("audio"))
    {
        "seçilen ses"
    } else if lower.contains("alt liste")
        || lower.contains("representation")
        || lower.contains("variant playlist")
    {
        "alt liste"
    } else if lower.contains("medya analizi başarısız") {
        if media_manifest_url(&request.url) {
            "ana liste"
        } else {
            "keşif"
        }
    } else if lower.contains("ana liste")
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
}

impl DownloadFailureStages {
    fn from_inspection(inspection: &Value, request: &AddRequest, selector: &str) -> Self {
        let mut stages = Self::default();
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
        bail!("Çoklu ses seçimi eski tek ses seçimiyle birlikte kullanılamaz");
    }
    if !request.subtitle_tracks.is_empty()
        && (!request.subtitle_languages.is_empty() || request.subtitle_mode.is_some())
    {
        bail!("Açık altyazı seçimi eski altyazı seçimiyle birlikte kullanılamaz");
    }
    if !request.external_subtitles.is_empty()
        && (!request.subtitle_tracks.is_empty()
            || !request.subtitle_languages.is_empty()
            || request.subtitle_mode.is_some())
    {
        bail!("HTML track altyazıları ile çıkarıcı altyazıları aynı istekte karıştırılamaz");
    }
    if let Some(id) = &request.video_format_id {
        let video = info
            .formats
            .iter()
            .find(|format| format.id == *id && !stream_absent(format.video_codec.as_deref()))
            .ok_or_else(|| {
                anyhow!("Seçilen görüntü biçimi artık bulunmuyor; başka biçime otomatik geçilmedi")
            })?;
        if request
            .exact_height
            .is_some_and(|height| video.height != Some(height))
        {
            crate::bail_code!(crate::error_codes::MED_012);
        }
    } else if let Some(height) = request.exact_height {
        if !info
            .formats
            .iter()
            .any(|f| f.height == Some(height) && !stream_absent(f.video_codec.as_deref()))
        {
            crate::bail_code!(
                crate::error_codes::MED_012,
                "Seçilen {height}p kalitesi artık bulunmuyor; başka kaliteye otomatik geçilmedi"
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
                    "Seçilen ses parçasının dili değişti: {}",
                    selection.id
                );
            }
        }
    }
    if let Some(id) = &request.audio_format_id {
        let audio = exact_audio_format(info, id)?;
        if let Some(language) = &request.audio_language {
            if audio.language.as_deref().map(language_key) != Some(language_key(language)) {
                bail!("Seçilen ses parçasının dili değişti");
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
                "Aynı dil için manuel ve otomatik altyazı birlikte veya yinelenerek seçilemez: {}",
                selection.language
            );
        }
        if !info.subtitle_tracks.iter().any(|track| {
            track.language == selection.language && track.automatic == selection.automatic
        }) {
            crate::bail_code!(
                crate::error_codes::MED_012,
                "Seçilen altyazı artık bulunmuyor: {}",
                selection.language
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
                "Seçilen altyazı bulunamadı: {language}"
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
        .ok_or_else(|| anyhow!("Seçilen ses parçası artık bulunmuyor: {id}"))
}

fn validate_audio_selection(selection: &AudioSelection) -> Result<()> {
    validate_format_id(&selection.id, "Ses format kimliği")?;
    if let Some(language) = &selection.language {
        validate_language(language, "Ses dili")?;
    }
    Ok(())
}

fn validate_subtitle_selection(selection: &SubtitleSelection) -> Result<()> {
    validate_language(&selection.language, "Altyazı dili")
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
            bail!("Çoklu ses seçimi yalnız video çıktısında kullanılabilir");
        }
        if let Some(selected) = request
            .video_format_id
            .as_ref()
            .and_then(|id| info.formats.iter().find(|format| format.id == *id))
        {
            if stream_present(selected.audio_codec.as_deref()) {
                bail!("Ayrı ses parçaları yalnız ses içermeyen bir görüntü biçimiyle birleştirilebilir");
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
            return Ok(format!("{video}+{}", best_mp4_audio(request, info)?.id));
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
            .context("Seçilen görüntü biçimi bulunamadı")?;
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
        bail!("Authorization veya Cookie başlığı tek medya başlık bloğuyla ana kaynak, alt liste ve parça sunucularına güvenle sınırlandırılamaz. Kaynak kapsamlı oturum çerezleri veya imzalı bağlantı kullanın.");
    }
    Ok(())
}

fn validate_format_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        bail!("{label} geçersiz");
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
        bail!("{label} geçersiz");
    }
    Ok(())
}

fn validate_external_subtitles(values: &[ExternalSubtitle]) -> Result<()> {
    if values.iter().filter(|subtitle| subtitle.is_default).count() > 1 {
        bail!("Yalnız bir harici altyazı varsayılan olabilir");
    }
    let mut identities = BTreeSet::new();
    for subtitle in values {
        validate_web_url(&subtitle.url)?;
        validate_language(&subtitle.language, "Harici altyazı dili")?;
        if !matches!(subtitle.kind.as_str(), "subtitles" | "captions") {
            bail!("Harici track türü yalnız subtitles veya captions olabilir");
        }
        if subtitle.label.len() > 200
            || subtitle
                .label
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
        {
            bail!("Harici altyazı etiketi geçersiz");
        }
        crate::validation::validate_headers(&subtitle.headers)?;
        reject_unsafe_media_headers(&subtitle.headers)?;
        if let Some(referer) = &subtitle.referer {
            validate_web_url(referer)?;
        }
        if !identities.insert((&subtitle.url, &subtitle.language, &subtitle.kind)) {
            bail!("Aynı harici altyazı birden fazla kez seçilemez");
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
        other => bail!("Desteklenmeyen ses çıktısı: {other}. Kullanılabilir seçenekler: original, MP3, M4A, Opus, FLAC"),
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
            bail!("Geçersiz altyazı dili seçimi: {value}");
        }
        valid.push(value);
    }
    Ok(valid.join(","))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod output_validation_tests {
    use super::*;
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
        available_qualities, format_unavailable, fragment_loss, iso639_2, tag_language,
        useful_error,
    };

    #[test]
    fn a_lost_fragment_is_recognized_and_a_clean_run_is_not() {
        assert!(fragment_loss(
            "[download] fragment not found; Skipping fragment 5 ...\n[download] Got error: HTTP Error 429"
        ));
        assert!(fragment_loss("[download] Skipping fragment 12"));
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
