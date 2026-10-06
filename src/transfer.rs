use crate::{
    model::{AddRequest, TransferControl, TransferProgress, TransferResult},
    network::{GovernedEasy, NetworkGovernor},
};
use anyhow::{anyhow, bail, Context, Result};
use curl::easy::List;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MIN_SEGMENT_SIZE: u64 = 2 * 1024 * 1024;
const MAX_SEGMENT_SIZE: u64 = 8 * 1024 * 1024;
const MIN_SPLIT_INTERVAL: Duration = Duration::from_millis(25);
const MAX_RANGE_ATTEMPTS: u8 = 3;
const RANGE_WORKER_IDLE_TIMEOUT: Duration = Duration::from_millis(200);
const IO_BUFFER: usize = 1024 * 1024;
const STATE_VERSION: u8 = 3;

#[derive(Debug, Clone)]
pub(crate) struct RemoteInfo {
    url: Url,
    credential_origin: (String, String, u16),
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    accept_ranges: bool,
    content_type: Option<String>,
    filename: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ResponseHeaders {
    status: u32,
    location: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    filename: Option<String>,
    content_length: Option<u64>,
    content_range: Option<ContentRange>,
    accept_ranges: bool,
    retry_after: Option<Duration>,
    headers_complete: bool,
}

#[derive(Debug, Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct ByteRange {
    /// Inclusive byte offset.
    start: u64,
    /// Exclusive byte offset.
    end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ResumeState {
    version: u8,
    /// Version 2 used this URL digest as part of identity. Version 3 retains it
    /// only for explicit, safe migration of old state.
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
    segmented: bool,
    connections: u8,
    checksum: Option<String>,
    /// Durably accepted, nonoverlapping ranges in the sparse part file.
    completed: Vec<ByteRange>,
}

impl Default for ResumeState {
    fn default() -> Self {
        Self {
            version: 0,
            url: String::new(),
            etag: None,
            last_modified: None,
            total: None,
            segmented: false,
            connections: 1,
            checksum: None,
            completed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExpectedResponse {
    Full,
    Range {
        start: u64,
        end: Option<u64>,
        total: Option<u64>,
    },
}

#[derive(Debug)]
enum AttemptFailure {
    Interrupted,
    RangeRejected,
    ResourceChanged,
    RetryableStatus {
        status: u32,
        retry_after: Option<Duration>,
    },
    RetryableTransport(anyhow::Error),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for AttemptFailure {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

#[derive(Debug)]
struct RetryableStatusError {
    status: u32,
    retry_after: Option<Duration>,
}

impl std::fmt::Display for RetryableStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after {
            Some(delay) => write!(
                formatter,
                "{}",
                crate::i18n::ui_owned!(
                    format!(
                        "Sunucu HTTP {} yanıtını verdi (Retry-After {} sn)",
                        self.status,
                        delay.as_secs()
                    ),
                    format!(
                        "The server returned an HTTP {} response (Retry-After {} s)",
                        self.status,
                        delay.as_secs()
                    ),
                )
            ),
            None => write!(
                formatter,
                "{}",
                crate::i18n::ui_owned!(
                    format!("Sunucu HTTP {} yanıtını verdi", self.status),
                    format!("The server returned an HTTP {} response", self.status),
                )
            ),
        }
    }
}

impl std::error::Error for RetryableStatusError {}

fn retryable_status_error(status: u32, retry_after: Option<Duration>) -> anyhow::Error {
    anyhow!(RetryableStatusError {
        status,
        retry_after,
    })
}

pub(crate) fn retry_after(error: &anyhow::Error) -> Option<Duration> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RetryableStatusError>()
            .and_then(|status| status.retry_after)
    })
}

/// A final non-success HTTP status. The engine classifies it by `status`, never by the
/// localized sentence, so retry and source-renewal decisions do not depend on the
/// interface language.
#[derive(Debug)]
pub(crate) struct HttpStatusError {
    pub(crate) status: u32,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            crate::i18n::ui_owned!(
                format!("Sunucu HTTP {} yanıtını verdi", self.status),
                format!("The server returned an HTTP {} response", self.status),
            )
        )
    }
}

impl std::error::Error for HttpStatusError {}

/// Kept partial data whose source identity is unproven or changed. It stays on disk until
/// the user restarts or a renewed source proves compatibility, so the engine treats it as a
/// source-renewal candidate.
#[derive(Debug)]
pub(crate) struct PreservedPartialError {
    message: &'static str,
}

impl std::fmt::Display for PreservedPartialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PreservedPartialError {}

fn preserved_partial(message: &'static str) -> anyhow::Error {
    anyhow!(PreservedPartialError { message })
}

/// The HTTP status carried anywhere in the error chain.
pub(crate) fn http_status(error: &anyhow::Error) -> Option<u32> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<HttpStatusError>()
            .map(|value| value.status)
            .or_else(|| {
                cause
                    .downcast_ref::<RetryableStatusError>()
                    .map(|value| value.status)
            })
    })
}

/// Whether the chain carries a kept partial file awaiting a proven source.
pub(crate) fn preserves_partial(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<PreservedPartialError>().is_some())
}

pub(crate) fn has_artifacts(output: &Path) -> bool {
    output.exists()
        || crate::media::has_partial_output(output)
        || part_path(output).exists()
        || state_path(output).exists()
        || (0..16).any(|index| segment_path(output, index).exists())
}

pub(crate) fn validate_source_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).context(crate::i18n::ui(
        "Kaynak adresi geçerli değil",
        "The source URL is not valid",
    ))?;
    match url.scheme() {
        "http" | "https" | "ftp" => {}
        _ => bail!(
            "{}",
            crate::i18n::ui(
                "Yalnızca HTTP, HTTPS ve FTP adresleri desteklenir",
                "Only HTTP, HTTPS and FTP addresses are supported",
            )
        ),
    }
    if url.host_str().is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Kaynak adresinde sunucu adı yok",
                "The source address has no host name",
            )
        );
    }
    if matches!(url.scheme(), "http" | "https")
        && (!url.username().is_empty() || url.password().is_some())
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "HTTP adresindeki kullanıcı bilgileri desteklenmez; yetkilendirme başlığı kullanın",
                "Credentials in HTTP addresses are not supported; use the authorization header",
            )
        );
    }
    Ok(url)
}

/// Auto jobs are sent to the media pipeline only on an explicit media URL or
/// an actual HTTP content type. A failed HEAD does not turn a normal file into
/// media based on a guess.
pub(crate) fn prepare_auto(
    request: &AddRequest,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<(bool, Option<RemoteInfo>)> {
    let url = validate_source_url(&request.url)?;
    // A selected stream, audio, subtitle, container, or playlist must stay in
    // the media pipeline so its mux and verification contract cannot be skipped.
    if request_requires_media_pipeline(request) || media_extension(&url) {
        return Ok((true, None));
    }
    if url.scheme() == "ftp" {
        return Ok((false, None));
    }
    let info = probe_http(request, url, control, network)?;
    let media = info
        .content_type
        .as_deref()
        .map(is_media_content_type)
        .unwrap_or(false);
    Ok((media, Some(info)))
}

#[derive(Debug)]
pub(crate) struct PreparedSourceRefresh {
    state_path: Option<PathBuf>,
    state: Option<ResumeState>,
}

/// Performs the cancellable remote identity check without changing retained work.
/// The actor must revalidate its refresh grant before calling `stage_source_refresh`.
pub(crate) fn prepare_source_refresh(
    previous: &AddRequest,
    next: &AddRequest,
    output: &Path,
    restart: bool,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<PreparedSourceRefresh> {
    if request_requires_media_pipeline(previous) || request_requires_media_pipeline(next) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Seçili medya akışı doğrudan dosya yenileme yoluna geçirilemez",
                "The selected media stream cannot use the direct file-refresh path",
            )
        );
    }
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("Kaynak yenileme durduruldu", "Source refresh stopped",)
        );
    }
    if restart
        || !part_path(output).exists()
            && !state_path(output).exists()
            && !legacy_segments_exist(output)
    {
        return Ok(PreparedSourceRefresh {
            state_path: None,
            state: None,
        });
    }
    let mut state = read_state(&state_path(output)).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Kısmi dosyaların kaynak kimliği kaydı yok; eserler korundu",
                "The partial files have no source identity record; the artifacts were preserved"
            )
        )
    })?;
    let next_url = validate_source_url(&next.url)?;
    if !matches!(next_url.scheme(), "http" | "https") {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yenilenen doğrudan kaynak için güçlü HTTP içerik kimliği gerekli",
                "Refreshing a direct source requires a strong HTTP content identity",
            )
        );
    }
    let remote = probe_http(next, next_url, control, network)?;
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("Kaynak yenileme durduruldu", "Source refresh stopped",)
        );
    }
    if state.total != remote.total {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yenilenen kaynağın boyutu değişti; kısmi dosyalar korundu",
                "The refreshed source changed size; partial files were kept",
            )
        );
    }
    let same_strong_etag =
        state
            .etag
            .as_deref()
            .zip(remote.etag.as_deref())
            .and_then(|(old, new)| {
                if is_strong_etag(old) && is_strong_etag(new) {
                    Some(old == new)
                } else {
                    None
                }
            });
    let previous_checksum = normalized_checksum(previous.checksum.as_deref());
    let next_checksum = normalized_checksum(next.checksum.as_deref());
    let same_checksum = state.version == STATE_VERSION
        && previous_checksum.is_some()
        && previous_checksum == next_checksum
        && state.checksum == next_checksum;
    if same_strong_etag == Some(false) || (same_strong_etag != Some(true) && !same_checksum) {
        bail!("{}",
            crate::i18n::ui(
                "Yenilenen kaynağın güçlü ETag veya tam SHA-256 kimliği eşleşmedi; kısmi dosyalar korundu",
                "The refreshed source's strong ETag or full SHA-256 identity did not match; partial files were kept",
            ));
    }
    state.url = source_url_digest(&remote.url);
    state.etag = remote.etag;
    state.last_modified = remote.last_modified;
    state.checksum = next_checksum;
    Ok(PreparedSourceRefresh {
        state_path: Some(state_path(output)),
        state: Some(state),
    })
}

/// Applies only the bounded local state update prepared by `prepare_source_refresh`.
pub(crate) fn stage_source_refresh(prepared: &PreparedSourceRefresh) -> Result<bool> {
    match (&prepared.state_path, &prepared.state) {
        (Some(path), Some(state)) => {
            write_state(path, state)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(crate) fn download(
    request: &AddRequest,
    output: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    prepared: Option<RemoteInfo>,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let url = validate_source_url(&request.url)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| {
            crate::i18n::ui_owned!(
                format!("Hedef klasör oluşturulamadı: {}", parent.display()),
                format!(
                    "The destination folder could not be created: {}",
                    parent.display()
                )
            )
        })?;
    }
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        );
    }

    if url.scheme() == "ftp" {
        download_ftp(request, output, url, control, network, progress)
    } else {
        download_http(
            request,
            output,
            prepared
                .map(Ok)
                .unwrap_or_else(|| probe_http(request, url, control, network))?,
            control,
            network,
            connections.clamp(1, 16),
            progress,
        )
    }
}

fn download_http(
    request: &AddRequest,
    output: &Path,
    remote: RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    // Dynamic ranges are reusable only with content identity, not merely a URL,
    // length, or timestamp. A caller-supplied full SHA-256 is proof only when
    // it does not conflict with present strong validators; mixed bytes can never
    // be published unless the final digest matches.
    let stable = has_resume_identity(&remote, request.checksum.as_deref());
    let segment_count = remote
        .total
        .map(|total| {
            (total / MIN_SEGMENT_SIZE)
                .max(1)
                .min(u64::from(connections)) as u8
        })
        .unwrap_or(1);

    let existing_state = read_state(&state_path(output));
    let sequential_resume = existing_state.as_ref().is_some_and(|state| {
        state.version == STATE_VERSION && !state.segmented && part_path(output).exists()
    });
    let empty_dynamic_allocation = existing_state.as_ref().is_some_and(|state| {
        state.version == STATE_VERSION
            && state.segmented
            && state.completed.is_empty()
            && state_matches(
                state,
                &remote,
                true,
                state.connections,
                normalized_checksum(request.checksum.as_deref()).as_deref(),
            )
    });
    let reusable_ranges = existing_state
        .as_ref()
        .map(|state| {
            let has_completed = !state.completed.is_empty()
                || (state.version == 2
                    && (part_path(output).exists() || legacy_segments_exist(output)));
            has_completed
                && (state.version == 2 || state.segmented)
                && (state_matches(
                    state,
                    &remote,
                    state.segmented,
                    state.connections,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                ) || legacy_state_matches(state, &remote))
        })
        .unwrap_or(false);
    let range_supported = if reusable_ranges {
        true
    } else if !sequential_resume
        && stable
        && remote.total.is_some()
        && remote.accept_ranges
        && segment_count > 1
    {
        match verify_range_support(request, &remote, control, network) {
            Ok(supported) => supported,
            Err(AttemptFailure::ResourceChanged) => {
                return Err(preserved_partial(crate::i18n::ui(
"Kaynak kimliği değişti; doğrulanmış parçalar yeniden başlatma seçilene kadar korundu",
                        "The source identity changed; verified parts are kept until you choose to restart",
)))
            }
            Err(AttemptFailure::Other(error)) => return Err(error),
            // Probe transport and status failures have always meant that range support
            // is unproven, so retain the existing single-transfer fallback.
            Err(_) => false,
        }
    } else {
        false
    };
    if !sequential_resume
        && stable
        && remote.total.is_some()
        && remote.accept_ranges
        && range_supported
    {
        match download_segmented(
            request,
            output,
            &remote,
            control,
            network,
            segment_count,
            progress,
        ) {
            Ok(result) => return Ok(result),
            Err(AttemptFailure::RangeRejected)
                if read_state(&state_path(output)).is_some_and(|state| {
                    state.version == STATE_VERSION
                        && state.segmented
                        && state.completed.is_empty()
                        && state_matches(
                            &state,
                            &remote,
                            true,
                            connections,
                            normalized_checksum(request.checksum.as_deref()).as_deref(),
                        )
                }) =>
            {
                // No range was durably accepted, so truncating the allocated sparse
                // file cannot discard resumable data or combine different resources.
                return download_http_single(
                    request, output, &remote, control, network, progress, false,
                );
            }
            Err(AttemptFailure::RangeRejected) => {
                return Err(preserved_partial(crate::i18n::ui(
"Sunucu byte aralığını reddetti; doğrulanmış parçalar yeniden başlatma seçilene kadar korundu",
                        "The server rejected the byte range; verified parts are kept until you choose to restart",
)))
            }
            Err(AttemptFailure::ResourceChanged) => {
                return Err(preserved_partial(crate::i18n::ui(
"Kaynak kimliği değişti; doğrulanmış parçalar yeniden başlatma seçilene kadar korundu",
                        "The source identity changed; verified parts are kept until you choose to restart",
)))
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) => return Err(retryable_status_error(status, retry_after)),
            Err(AttemptFailure::RetryableTransport(error)) | Err(AttemptFailure::Other(error)) => {
                return Err(error)
            }
            Err(AttemptFailure::Interrupted) => bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped")
            ),
        }
    }

    if empty_dynamic_allocation {
        return download_http_single(request, output, &remote, control, network, progress, false);
    }
    download_http_single(request, output, &remote, control, network, progress, true)
}

fn download_http_single(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
    permit_resume: bool,
) -> Result<TransferResult> {
    let part = part_path(output);
    let state_path = state_path(output);
    let previous = read_state(&state_path);
    let part_bytes = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let restart_empty_dynamic = !permit_resume
        && remote.total == Some(part_bytes)
        && previous.as_ref().is_some_and(|state| {
            state.version == STATE_VERSION
                && state.segmented
                && state.completed.is_empty()
                && state_matches(
                    state,
                    remote,
                    true,
                    state.connections,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                )
        });
    let offset = if restart_empty_dynamic { 0 } else { part_bytes };
    let resumable = permit_resume
        && offset > 0
        && previous
            .as_ref()
            .map(|state| {
                state_matches(
                    state,
                    remote,
                    false,
                    1,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                )
            })
            .unwrap_or(false)
        && remote.total.map(|total| offset <= total).unwrap_or(true)
        && remote.accept_ranges;
    if !resumable && offset > 0 {
        return Err(preserved_partial(crate::i18n::ui(
"Kısmi dosyanın kaynak kimliği kanıtlanamadı; yeniden başlatma seçilene kadar korundu",
                "The partial file's source identity could not be proven; it is kept until you choose to restart",
)));
    }
    if remote.total == Some(offset) && offset > 0 && resumable {
        return finalize_part(
            &final_name(output, request, remote, None),
            &part,
            &state_path,
            offset,
            control,
            request.checksum.as_deref(),
        );
    }
    if restart_empty_dynamic {
        let file = OpenOptions::new()
            .write(true)
            .open(&part)
            .with_context(|| {
                crate::i18n::ui_owned!(
                    format!("Geçici dosya açılamadı: {}", part.display()),
                    format!("The temporary file could not be opened: {}", part.display())
                )
            })?;
        file.set_len(0)?;
        file.sync_all()?;
    }

    write_state(
        &state_path,
        &ResumeState {
            version: STATE_VERSION,
            url: hex::encode(Sha256::digest(remote.url.as_str().as_bytes())),
            etag: remote.etag.clone(),
            last_modified: remote.last_modified.clone(),
            total: remote.total,
            segmented: false,
            connections: 1,
            checksum: normalized_checksum(request.checksum.as_deref()),
            completed: Vec::new(),
        },
    )?;

    let mut current = remote.url.clone();
    let original_origin = remote.credential_origin.clone();
    let mut redirects = 0usize;
    let mut retries = 0u8;
    loop {
        let expected = if offset > 0 {
            ExpectedResponse::Range {
                start: offset,
                end: None,
                total: remote.total,
            }
        } else {
            ExpectedResponse::Full
        };
        let attempt = http_to_file(
            request,
            &current,
            &original_origin,
            &part,
            offset,
            expected,
            remote,
            control,
            network,
            1,
            progress,
        );
        match attempt {
            Ok((headers, written)) => {
                if is_redirect(headers.status) {
                    if redirects == MAX_REDIRECTS {
                        bail!(
                            "{}",
                            crate::i18n::ui(
                                "Çok fazla HTTP yönlendirmesi",
                                "Too many HTTP redirects",
                            )
                        );
                    }
                    redirects += 1;
                    current = redirected_url(&current, headers.location.as_deref())?;
                    continue;
                }
                let bytes = offset.saturating_add(written);
                if let Some(total) = remote.total {
                    if bytes != total {
                        bail!(
                            "{}",
                            crate::i18n::ui_owned!(
                                format!("Aktarım eksik kaldı: {bytes}/{total} bayt"),
                                format!("Transfer incomplete: {bytes}/{total} bytes"),
                            )
                        );
                    }
                }
                return finalize_part(
                    &final_name(output, request, remote, Some(&headers)),
                    &part,
                    &state_path,
                    bytes,
                    control,
                    request.checksum.as_deref(),
                );
            }
            Err(AttemptFailure::RangeRejected | AttemptFailure::ResourceChanged) if offset > 0 => {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Sürdürme yanıtı kaynak kimliğini kanıtlamadı; kısmi dosya korundu",
                        "The resume response did not prove the source identity; the partial file was kept",
                    )
                )
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) if retries + 1 < MAX_RANGE_ATTEMPTS => {
                let delay = retry_after.unwrap_or_else(|| range_retry_delay(retries));
                progress(TransferProgress {
                    downloaded: offset,
                    total: remote.total,
                    speed: 0,
                    eta: None,
                    phase: crate::i18n::ui_owned!(
                        format!(
                            "HTTP {status}; {} sn sonra tek bağlantıyla yeniden denenecek",
                            delay.as_secs()
                        ),
                        format!(
                            "HTTP {status}; retrying single connection in {} s",
                            delay.as_secs()
                        )
                    ),
                });
                wait_for_retry(control, delay)?;
                retries += 1;
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) => return Err(retryable_status_error(status, retry_after)),
            Err(AttemptFailure::RetryableTransport(_)) if retries + 1 < MAX_RANGE_ATTEMPTS => {
                let delay = range_retry_delay(retries);
                progress(TransferProgress {
                    downloaded: offset,
                    total: remote.total,
                    speed: 0,
                    eta: None,
                    phase: crate::i18n::ui_owned!(
                        format!(
                            "Geçici ağ hatası; {} sn sonra tek bağlantıyla yeniden denenecek",
                            delay.as_secs()
                        ),
                        format!(
                            "Transient network error; retrying single connection in {} s",
                            delay.as_secs()
                        )
                    ),
                });
                wait_for_retry(control, delay)?;
                retries += 1;
            }
            Err(AttemptFailure::Interrupted) => bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped")
            ),
            Err(AttemptFailure::RetryableTransport(error)) | Err(AttemptFailure::Other(error)) => {
                return Err(error)
            }
            Err(AttemptFailure::RangeRejected | AttemptFailure::ResourceChanged) => {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Sunucu geçersiz veya değişmiş bir yanıt gönderdi",
                        "The server sent an invalid or changed response",
                    )
                )
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn http_to_file(
    request: &AddRequest,
    url: &Url,
    original_origin: &(String, String, u16),
    path: &Path,
    offset: u64,
    expected: ExpectedResponse,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    divisor: u64,
    progress: &mut dyn FnMut(TransferProgress),
) -> std::result::Result<(ResponseHeaders, u64), AttemptFailure> {
    let mut easy = network
        .easy_for_url(url, control)
        .map_err(AttemptFailure::Other)?;
    configure_easy(&mut easy, url)?;
    easy.follow_location(false).map_err(anyhow::Error::from)?;
    if let ExpectedResponse::Range { start, end, .. } = expected {
        let range = end
            .map(|last| format!("{start}-{last}"))
            .unwrap_or_else(|| format!("{start}-"));
        easy.range(&range).map_err(anyhow::Error::from)?;
    }
    let headers_list = build_headers(
        request,
        url,
        original_origin,
        match expected {
            ExpectedResponse::Range { .. } => range_validator(remote),
            _ => None,
        },
    )?;
    easy.http_headers(headers_list)
        .map_err(anyhow::Error::from)?;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .with_context(|| {
            crate::i18n::ui_owned!(
                format!("Geçici dosya açılamadı: {}", path.display()),
                format!("The temporary file could not be opened: {}", path.display())
            )
        })
        .map_err(AttemptFailure::Other)?;
    if offset == 0 {
        file.set_len(0)
            .map_err(|e| AttemptFailure::Other(e.into()))?;
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| AttemptFailure::Other(e.into()))?;

    let response = Rc::new(RefCell::new(ResponseHeaders::default()));
    let callback_response = response.clone();
    let write_response = response.clone();
    let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
    let write_error = io_error.clone();
    let mut written = 0u64;
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut last_bytes = offset;
    let mut throttle_started = Instant::now();
    let mut throttle_bytes = 0u64;

    easy.progress(true).map_err(anyhow::Error::from)?;
    let perform_result = {
        let mut transfer = easy.transfer();
        transfer
            .header_function(move |line| {
                parse_header_line(&mut callback_response.borrow_mut(), line);
                true
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .write_function(|data| {
                if control.stop_requested() {
                    return Ok(0);
                }
                if !response_allows_remote_body(&write_response.borrow(), expected, remote) {
                    return Ok(0);
                }
                if let Err(error) = file.write_all(data) {
                    *write_error.borrow_mut() = Some(error);
                    return Ok(0);
                }
                written = written.saturating_add(data.len() as u64);
                throttle_bytes = throttle_bytes.saturating_add(data.len() as u64);
                throttle(control, divisor, &mut throttle_started, &mut throttle_bytes);
                let now = Instant::now();
                if now.duration_since(last_report) >= Duration::from_millis(200) {
                    let elapsed = now.duration_since(last_report).as_secs_f64().max(0.001);
                    let current = offset.saturating_add(written);
                    let speed = ((current.saturating_sub(last_bytes)) as f64 / elapsed) as u64;
                    let total = remote.total;
                    progress(TransferProgress {
                        downloaded: current,
                        total,
                        speed,
                        eta: eta(total, current, speed),
                        phase: crate::i18n::ui("İndiriliyor", "Downloading").into(),
                    });
                    last_report = now;
                    last_bytes = current;
                }
                Ok(data.len())
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .progress_function(|_, _, _, _| !control.stop_requested())
            .map_err(anyhow::Error::from)?;
        transfer.perform()
    };

    let headers = response.borrow().clone();
    if control.stop_requested() {
        file.sync_all().map_err(|error| {
            AttemptFailure::Other(anyhow!(error).context(crate::i18n::ui(
                "Duraklatılan kısmi dosya diske yazılamadı",
                "The paused partial file could not be written to disk",
            )))
        })?;
        return Err(AttemptFailure::Interrupted);
    }
    if let Some(error) = io_error.borrow_mut().take() {
        return Err(AttemptFailure::Other(anyhow!(error).context(
            crate::i18n::ui(
                "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
                "The file could not be written; the disk is full or access was denied",
            ),
        )));
    }
    if is_redirect(headers.status) {
        return Ok((headers, 0));
    }
    if matches!(headers.status, 429 | 503) {
        return Err(AttemptFailure::RetryableStatus {
            status: headers.status,
            retry_after: headers.retry_after,
        });
    }
    if matches!(
        headers.status,
        401 | 403 | 404 | 408 | 410 | 500 | 502 | 504
    ) {
        return Err(AttemptFailure::Other(anyhow!(HttpStatusError {
            status: headers.status
        })));
    }
    if headers.status == 0 {
        return match perform_result {
            Err(error) => Err(AttemptFailure::RetryableTransport(
                governed_transport_error(
                    &easy,
                    error,
                    crate::i18n::ui(
                        "Ağ aktarımı tamamlanamadı",
                        "The network transfer did not complete",
                    ),
                ),
            )),
            Ok(()) => Err(AttemptFailure::RangeRejected),
        };
    }
    if validators_changed(&headers, remote) {
        return Err(AttemptFailure::ResourceChanged);
    }
    if !response_allows_body(&headers, expected) {
        return Err(AttemptFailure::RangeRejected);
    }
    if let Err(error) = perform_result {
        let error = governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "Ağ aktarımı tamamlanamadı",
                "The network transfer did not complete",
            ),
        );
        if written > 0 {
            file.sync_all().map_err(|sync_error| {
                AttemptFailure::Other(anyhow!(sync_error).context(crate::i18n::ui(
                    "Kısmi dosya diske yazılamadı",
                    "The partial file could not be written to disk",
                )))
            })?;
            return Err(AttemptFailure::Other(error.context(crate::i18n::ui(
                "Aktarım kesildi; kısmi dosya korundu",
                "The transfer was interrupted; the partial file was kept",
            ))));
        }
        return Err(AttemptFailure::RetryableTransport(error));
    }
    file.flush().map_err(|e| {
        AttemptFailure::Other(anyhow!(e).context(crate::i18n::ui(
            "Geçici dosya yazılamadı",
            "The temporary file could not be written",
        )))
    })?;
    let current = offset.saturating_add(written);
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let speed = (written as f64 / elapsed) as u64;
    progress(TransferProgress {
        downloaded: current,
        total: remote.total,
        speed,
        eta: eta(remote.total, current, speed),
        phase: crate::i18n::ui("İndiriliyor", "Downloading").into(),
    });
    Ok((headers, written))
}

#[derive(Debug, Clone, Copy)]
struct RangeWork {
    range: ByteRange,
    attempts: u8,
}

enum RangeCommand {
    Download(RangeWork),
    Stop,
}

enum RangeEvent {
    Progress {
        worker: usize,
        bytes: u64,
    },
    Complete {
        worker: usize,
        work: RangeWork,
        result: std::result::Result<(), AttemptFailure>,
    },
}

fn download_segmented(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    count: u8,
    progress: &mut dyn FnMut(TransferProgress),
) -> std::result::Result<TransferResult, AttemptFailure> {
    // This is the connection allocation granted by the engine. Dynamic workers
    // never exceed it, even if this private entry point is reused later.
    let count = count.clamp(1, 16);
    let total = remote
        .total
        .ok_or_else(|| AttemptFailure::Other(anyhow!("Dosya boyutu bilinmiyor")))?;
    let part = part_path(output);
    let state_path = state_path(output);
    let mut state = prepare_dynamic_state(request, output, remote, total, count)?;
    let mut completed = normalize_completed(&state.completed, total)?;
    state.completed = completed.clone();
    write_state(&state_path, &state).map_err(AttemptFailure::Other)?;

    let mut pending = missing_ranges(total, &completed);
    let baseline = completed.iter().map(|range| range.len()).sum::<u64>();
    crate::output::require_space(output, total.saturating_sub(baseline))
        .map_err(AttemptFailure::Other)?;
    if baseline == total {
        return finalize_part(
            &final_name(output, request, remote, None),
            &part,
            &state_path,
            total,
            control,
            request.checksum.as_deref(),
        )
        .map_err(AttemptFailure::Other);
    }

    let worker_count = usize::from(count.max(1));
    let lease_size = total
        .div_ceil(u64::from(count.max(1)))
        .clamp(MIN_SEGMENT_SIZE, MAX_SEGMENT_SIZE);
    let (event_tx, event_rx) = mpsc::sync_channel::<RangeEvent>(worker_count * 2);
    let mut senders = Vec::with_capacity(worker_count);
    let mut handles = Vec::with_capacity(worker_count);
    for worker in 0..worker_count {
        let (command_tx, command_rx) = mpsc::sync_channel::<RangeCommand>(1);
        senders.push(command_tx);
        let events = event_tx.clone();
        let request = request.clone();
        let remote = remote.clone();
        let part = part.clone();
        let control = control.clone();
        let network = NetworkGovernor::clone(network);
        handles.push(thread::spawn(move || {
            let mut downloader = DynamicRangeWorker {
                easy: None,
                network,
                request: &request,
                remote: &remote,
                path: &part,
                control: &control,
                divisor: count as u64,
            };
            loop {
                let command = match command_rx.recv_timeout(RANGE_WORKER_IDLE_TIMEOUT) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // A scheduler can leave a completed worker idle while another
                        // worker waits for its first socket. Release this worker's
                        // pooled socket, but retain the handle for consecutive ranges.
                        drop(downloader.easy.take());
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let RangeCommand::Download(work) = command else {
                    break;
                };
                let updates = events.clone();
                let result = downloader.download(work.range, &mut |bytes| {
                    let _ = updates.try_send(RangeEvent::Progress { worker, bytes });
                });
                if events
                    .send(RangeEvent::Complete {
                        worker,
                        work,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    drop(event_tx);

    let started = Instant::now();
    let mut idle = (0..worker_count).rev().collect::<Vec<_>>();
    let mut active = 0usize;
    let mut effective_workers = worker_count;
    let mut active_bytes = vec![0u64; worker_count];
    let mut retry_not_before = Instant::now();
    let mut last_split: Option<Instant> = None;
    let mut successful_since_failure = 0usize;
    let mut last_time = Instant::now();
    let mut last_downloaded = baseline;
    let outcome = 'scheduler: loop {
        if control.stop_requested() {
            break Err(AttemptFailure::Interrupted);
        }

        let now = Instant::now();
        while active < effective_workers && !idle.is_empty() && !pending.is_empty() {
            if now < retry_not_before {
                break;
            }
            let needs_split = pending
                .iter()
                .map(|work| work.range.len())
                .max()
                .is_some_and(|length| range_can_split(length, lease_size));
            if needs_split
                && last_split
                    .map(|last| last.elapsed() < MIN_SPLIT_INTERVAL)
                    .unwrap_or(false)
            {
                break;
            }
            let work = take_largest_range(&mut pending, lease_size);
            if needs_split {
                last_split = Some(Instant::now());
            }
            let worker = idle.pop().expect("idle worker disappeared");
            active_bytes[worker] = 0;
            if senders[worker].send(RangeCommand::Download(work)).is_err() {
                break 'scheduler Err(AttemptFailure::Other(anyhow!(
                    "İndirme işçisi beklenmedik biçimde kapandı"
                )));
            }
            active += 1;
        }

        if pending.is_empty() && active == 0 {
            break Ok(());
        }

        let now = Instant::now();
        let split_wait = last_split
            .map(|last| MIN_SPLIT_INTERVAL.saturating_sub(now.duration_since(last)))
            .unwrap_or(Duration::ZERO);
        let retry_wait = retry_not_before.saturating_duration_since(now);
        let can_dispatch = active < effective_workers && !idle.is_empty() && !pending.is_empty();
        let wait = if can_dispatch {
            // A live worker must not turn the split interval into a 200 ms
            // dispatch delay; wake as soon as the next eligible lease may start.
            split_wait.max(retry_wait).min(Duration::from_millis(200))
        } else {
            Duration::from_millis(200)
        };
        match event_rx.recv_timeout(wait) {
            Ok(RangeEvent::Progress { worker, bytes }) => {
                active_bytes[worker] = bytes;
            }
            Ok(RangeEvent::Complete {
                worker,
                work,
                result,
            }) => {
                active = active.saturating_sub(1);
                active_bytes[worker] = 0;
                idle.push(worker);
                match result {
                    Ok(()) => {
                        completed.push(work.range);
                        completed = match normalize_completed(&completed, total) {
                            Ok(value) => value,
                            Err(error) => break 'scheduler Err(error),
                        };
                        state.completed = completed.clone();
                        // The range worker syncs the data first. Only then is
                        // the atomic state allowed to claim those bytes.
                        if let Err(error) = write_state(&state_path, &state) {
                            break 'scheduler Err(AttemptFailure::Other(error));
                        }
                        successful_since_failure += 1;
                        if effective_workers < worker_count
                            && successful_since_failure >= effective_workers.max(1) * 2
                        {
                            effective_workers += 1;
                            successful_since_failure = 0;
                        }
                    }
                    Err(AttemptFailure::RetryableStatus {
                        status,
                        retry_after,
                    }) if work.attempts + 1 < MAX_RANGE_ATTEMPTS => {
                        let retry_after =
                            retry_after.unwrap_or_else(|| range_retry_delay(work.attempts));
                        let Some(not_before) = Instant::now().checked_add(retry_after) else {
                            break 'scheduler Err(AttemptFailure::Other(anyhow!(
                                "{}",
                                crate::i18n::ui(
                                    "Retry-After süresi desteklenen zaman aralığını aşıyor",
                                    "The Retry-After duration exceeds the supported time range",
                                )
                            )));
                        };
                        pending.push(RangeWork {
                            range: work.range,
                            attempts: work.attempts + 1,
                        });
                        effective_workers = (effective_workers / 2).max(1);
                        successful_since_failure = 0;
                        retry_not_before = retry_not_before.max(not_before);
                        progress(TransferProgress {
                            downloaded: completed.iter().map(|range| range.len()).sum(),
                            total: Some(total),
                            speed: 0,
                            eta: None,
                            phase: crate::i18n::ui_owned!(
                                format!(
                                    "HTTP {status}; {} sn sonra {} bağlantıyla yeniden denenecek",
                                    retry_after.as_secs(),
                                    effective_workers,
                                ),
                                format!(
                                    "HTTP {status}; retrying in {} s using {} connections",
                                    retry_after.as_secs(),
                                    effective_workers,
                                )
                            ),
                        });
                    }
                    Err(AttemptFailure::RetryableTransport(_error))
                        if work.attempts + 1 < MAX_RANGE_ATTEMPTS =>
                    {
                        let retry_after = range_retry_delay(work.attempts);
                        let Some(not_before) = Instant::now().checked_add(retry_after) else {
                            break 'scheduler Err(AttemptFailure::Other(anyhow!(
                                "{}",
                                crate::i18n::ui(
                                    "Geri çekilme süresi desteklenen zaman aralığını aşıyor",
                                    "The retry backoff duration exceeds the supported time range",
                                )
                            )));
                        };
                        pending.push(RangeWork {
                            range: work.range,
                            attempts: work.attempts + 1,
                        });
                        effective_workers = (effective_workers / 2).max(1);
                        successful_since_failure = 0;
                        retry_not_before = retry_not_before.max(not_before);
                        progress(TransferProgress {
                            downloaded: completed.iter().map(|range| range.len()).sum(),
                            total: Some(total),
                            speed: 0,
                            eta: None,
                            phase: crate::i18n::ui_owned!(
                                format!(
                                "Geçici ağ hatası; {} sn sonra {} bağlantıyla yeniden denenecek",
                                retry_after.as_secs(),
                                effective_workers,
                            ),
                                format!(
                                "Transient network error; retrying in {} s using {} connections",
                                retry_after.as_secs(),
                                effective_workers,
                            )
                            ),
                        });
                    }
                    Err(AttemptFailure::RetryableTransport(error)) => {
                        break 'scheduler Err(AttemptFailure::Other(error))
                    }
                    Err(error) => break Err(error),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(AttemptFailure::Other(anyhow!(
                    "{}",
                    crate::i18n::ui(
                        "Tüm indirme işçileri beklenmedik biçimde kapandı",
                        "All download workers closed unexpectedly",
                    )
                )))
            }
        }

        let now = Instant::now();
        if now.duration_since(last_time) >= Duration::from_millis(200) {
            let durable = completed.iter().map(|range| range.len()).sum::<u64>();
            let downloaded = durable.saturating_add(active_bytes.iter().sum::<u64>());
            let elapsed = now.duration_since(last_time).as_secs_f64().max(0.001);
            let speed = ((downloaded.saturating_sub(last_downloaded)) as f64 / elapsed) as u64;
            progress(TransferProgress {
                downloaded: downloaded.min(total),
                total: Some(total),
                speed,
                eta: eta(Some(total), downloaded.min(total), speed),
                // Shown after the state: "Downloading — 8 connections".
                phase: crate::i18n::ui_owned!(
                    format!("{effective_workers} bağlantı"),
                    if effective_workers == 1 {
                        "1 connection".to_string()
                    } else {
                        format!("{effective_workers} connections")
                    }
                ),
            });
            last_time = now;
            last_downloaded = downloaded;
        }
    };

    for sender in &senders {
        let _ = sender.try_send(RangeCommand::Stop);
    }
    drop(senders);
    for handle in handles {
        if handle.join().is_err() && outcome.is_ok() {
            return Err(AttemptFailure::Other(anyhow!(
                "{}",
                crate::i18n::ui(
                    "İndirme iş parçacığı beklenmedik biçimde kapandı",
                    "The download thread closed unexpectedly",
                )
            )));
        }
    }
    outcome?;

    let completed = normalize_completed(&completed, total)?;
    if completed
        != vec![ByteRange {
            start: 0,
            end: total,
        }]
    {
        return Err(AttemptFailure::Other(anyhow!(
            "{}",
            crate::i18n::ui(
                "Kalıcı aralık haritasında boşluk kaldı",
                "Gaps remained in the permanent range map",
            )
        )));
    }
    progress(TransferProgress {
        downloaded: total,
        total: Some(total),
        speed: ((total.saturating_sub(baseline)) as f64
            / started.elapsed().as_secs_f64().max(0.001)) as u64,
        eta: Some(0),
        phase: crate::i18n::ui("Tamamlanıyor", "Completing").into(),
    });
    cleanup_legacy_segments(output);
    finalize_part(
        &final_name(output, request, remote, None),
        &part,
        &state_path,
        total,
        control,
        request.checksum.as_deref(),
    )
    .map_err(AttemptFailure::Other)
}

struct DynamicRangeWorker<'a> {
    easy: Option<GovernedEasy>,
    network: NetworkGovernor,
    request: &'a AddRequest,
    remote: &'a RemoteInfo,
    path: &'a Path,
    control: &'a TransferControl,
    divisor: u64,
}

impl DynamicRangeWorker<'_> {
    fn download(
        &mut self,
        range: ByteRange,
        report: &mut dyn FnMut(u64),
    ) -> std::result::Result<(), AttemptFailure> {
        if self.easy.is_none() {
            let easy = self
                .network
                .easy_for_url(&self.remote.url, self.control)
                .map_err(AttemptFailure::Other)?;
            self.easy = Some(easy);
        }
        let easy = self.easy.as_mut().expect("range worker handle missing");
        let request = self.request;
        let remote = self.remote;
        let path = self.path;
        let control = self.control;
        let divisor = self.divisor;
        // Do not reset a governed handle: libcurl reset clears its socket callbacks.
        // Reapplying request options preserves callback accounting and lets
        // immediately assigned ranges reuse the connection.
        configure_easy(easy, &remote.url)?;
        easy.range(&format!("{}-{}", range.start, range.end - 1))
            .map_err(anyhow::Error::from)?;
        easy.progress(true).map_err(anyhow::Error::from)?;
        easy.http_headers(build_headers(
            request,
            &remote.url,
            &remote.credential_origin,
            range_validator(remote),
        )?)
        .map_err(anyhow::Error::from)?;

        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .open(path)
            .with_context(|| {
                crate::i18n::ui_owned!(
                    format!("Geçici dosya açılamadı: {}", path.display()),
                    format!("The temporary file could not be opened: {}", path.display())
                )
            })
            .map_err(AttemptFailure::Other)?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| AttemptFailure::Other(error.into()))?;

        let response = Rc::new(RefCell::new(ResponseHeaders::default()));
        let callback_response = response.clone();
        let write_response = response.clone();
        let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
        let write_error = io_error.clone();
        let mut written = 0u64;
        let mut last_report = Instant::now();
        let mut throttle_started = Instant::now();
        let mut throttle_bytes = 0u64;
        let expected = ExpectedResponse::Range {
            start: range.start,
            end: Some(range.end - 1),
            total: remote.total,
        };
        let perform_result = {
            let mut transfer = easy.transfer();
            transfer
                .header_function(move |line| {
                    parse_header_line(&mut callback_response.borrow_mut(), line);
                    true
                })
                .map_err(anyhow::Error::from)?;
            transfer
                .write_function(|data| {
                    if control.stop_requested() {
                        return Ok(0);
                    }
                    if !response_allows_remote_body(&write_response.borrow(), expected, remote) {
                        return Ok(0);
                    }
                    let remaining = range.len().saturating_sub(written);
                    if data.len() as u64 > remaining {
                        return Ok(0);
                    }
                    if let Err(error) = file.write_all(data) {
                        *write_error.borrow_mut() = Some(error);
                        return Ok(0);
                    }
                    written += data.len() as u64;
                    throttle_bytes += data.len() as u64;
                    throttle(control, divisor, &mut throttle_started, &mut throttle_bytes);
                    if last_report.elapsed() >= Duration::from_millis(200) {
                        report(written);
                        last_report = Instant::now();
                    }
                    Ok(data.len())
                })
                .map_err(anyhow::Error::from)?;
            transfer
                .progress_function(|_, _, _, _| !control.stop_requested())
                .map_err(anyhow::Error::from)?;
            transfer.perform()
        };

        let headers = response.borrow().clone();
        if control.stop_requested() {
            return Err(AttemptFailure::Interrupted);
        }
        if let Some(error) = io_error.borrow_mut().take() {
            return Err(AttemptFailure::Other(anyhow!(error).context(
                crate::i18n::ui(
                    "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
                    "The file could not be written; the disk is full or access was denied",
                ),
            )));
        }
        if matches!(headers.status, 429 | 503) {
            return Err(AttemptFailure::RetryableStatus {
                status: headers.status,
                retry_after: headers.retry_after,
            });
        }
        if headers.status == 0 {
            return match perform_result {
                Err(error) => Err(AttemptFailure::RetryableTransport(
                    governed_transport_error(
                        easy,
                        error,
                        crate::i18n::ui(
                            "Ağ aktarımı tamamlanamadı",
                            "The network transfer did not complete",
                        ),
                    ),
                )),
                Ok(()) => Err(AttemptFailure::RangeRejected),
            };
        }
        if validators_changed(&headers, remote) {
            return Err(AttemptFailure::ResourceChanged);
        }
        if !response_allows_body(&headers, expected) {
            return Err(AttemptFailure::RangeRejected);
        }
        if let Err(error) = perform_result {
            return Err(AttemptFailure::RetryableTransport(
                governed_transport_error(
                    easy,
                    error,
                    crate::i18n::ui(
                        "Ağ aktarımı tamamlanamadı",
                        "The network transfer did not complete",
                    ),
                ),
            ));
        }
        if written != range.len() {
            return Err(AttemptFailure::Other(anyhow!(
                "Byte aralığı eksik kaldı: {}/{}",
                written,
                range.len()
            )));
        }
        file.sync_all().map_err(|error| {
            AttemptFailure::Other(anyhow!(error).context(crate::i18n::ui(
                "Aralık diske yazılamadı",
                "The range could not be written to disk",
            )))
        })?;
        report(written);
        Ok(())
    }
}

fn prepare_dynamic_state(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    total: u64,
    connections: u8,
) -> std::result::Result<ResumeState, AttemptFailure> {
    let part = part_path(output);
    let state_path = state_path(output);
    let checksum = normalized_checksum(request.checksum.as_deref());
    let previous = read_state(&state_path);
    if let Some(state) = previous {
        if state.version == STATE_VERSION {
            if !state_matches(&state, remote, true, connections, checksum.as_deref()) {
                return Err(AttemptFailure::ResourceChanged);
            }
            if fs::metadata(&part).map(|value| value.len()).ok() != Some(total) {
                return Err(AttemptFailure::Other(anyhow!(
                    "Aralık haritasının seyrek dosyası eksik; mevcut eserler korundu"
                )));
            }
            return Ok(ResumeState {
                url: source_url_digest(&remote.url),
                etag: remote.etag.clone(),
                last_modified: remote.last_modified.clone(),
                connections,
                checksum,
                ..state
            });
        }
        if state.version == 2 && legacy_state_matches(&state, remote) {
            return migrate_v2_state(output, remote, state, checksum, connections);
        }
        return Err(AttemptFailure::ResourceChanged);
    }
    if part.exists() || legacy_segments_exist(output) {
        return Err(AttemptFailure::Other(anyhow!(
            "Kaydı olmayan kısmi dosyaların kimliği kanıtlanamadı; eserler korundu"
        )));
    }
    crate::output::require_space(output, total).map_err(AttemptFailure::Other)?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&part)
        .with_context(|| {
            crate::i18n::ui_owned!(
                format!("Geçici dosya oluşturulamadı: {}", part.display()),
                format!(
                    "The temporary file could not be created: {}",
                    part.display()
                )
            )
        })
        .map_err(AttemptFailure::Other)?;
    mark_sparse(&file);
    file.set_len(total).map_err(|error| {
        AttemptFailure::Other(anyhow!(error).context(crate::i18n::ui(
            "Seyrek geçici dosya ayrılamadı",
            "The sparse temporary file could not be allocated",
        )))
    })?;
    file.sync_all()
        .map_err(|error| AttemptFailure::Other(error.into()))?;
    Ok(ResumeState {
        version: STATE_VERSION,
        url: source_url_digest(&remote.url),
        etag: remote.etag.clone(),
        last_modified: remote.last_modified.clone(),
        total: Some(total),
        segmented: true,
        connections,
        checksum,
        completed: Vec::new(),
    })
}

/// Marks a range file sparse before it is extended, so unwritten ranges occupy no disk
/// space and a write far into the file does not make NTFS zero-fill everything before it.
/// Volumes without sparse support (FAT32, exFAT) keep the plain allocation.
fn mark_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    let mut returned = 0u32;
    unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

fn migrate_v2_state(
    output: &Path,
    remote: &RemoteInfo,
    old: ResumeState,
    checksum: Option<String>,
    connections: u8,
) -> std::result::Result<ResumeState, AttemptFailure> {
    let total = remote.total.expect("segmented transfer has total");
    let part = part_path(output);
    let mut completed = Vec::new();
    if old.segmented {
        if fs::metadata(&part).map(|value| value.len()).ok() == Some(total) {
            completed.push(ByteRange {
                start: 0,
                end: total,
            });
        } else {
            let mut target = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .read(true)
                .open(&part)
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            mark_sparse(&target);
            target
                .set_len(total)
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            let mut buffer = vec![0u8; IO_BUFFER];
            for (index, (start, end)) in
                split_ranges(total, old.connections).into_iter().enumerate()
            {
                let source_path = segment_path(output, index);
                let length = fs::metadata(&source_path)
                    .map(|value| value.len())
                    .unwrap_or(0);
                let maximum = end - start + 1;
                if length > maximum {
                    return Err(AttemptFailure::Other(anyhow!(
                        "Eski parça sınırını aşıyor; eserler korundu"
                    )));
                }
                if length == 0 {
                    continue;
                }
                let mut source = File::open(&source_path)
                    .map_err(|error| AttemptFailure::Other(error.into()))?;
                target
                    .seek(SeekFrom::Start(start))
                    .map_err(|error| AttemptFailure::Other(error.into()))?;
                let mut remaining = length;
                while remaining > 0 {
                    let amount = source
                        .read(&mut buffer[..remaining.min(IO_BUFFER as u64) as usize])
                        .map_err(|error| AttemptFailure::Other(error.into()))?;
                    if amount == 0 {
                        return Err(AttemptFailure::Other(anyhow!(
                            "Eski parça beklenmedik biçimde kısa"
                        )));
                    }
                    target
                        .write_all(&buffer[..amount])
                        .map_err(|error| AttemptFailure::Other(error.into()))?;
                    remaining -= amount as u64;
                }
                completed.push(ByteRange {
                    start,
                    end: start + length,
                });
            }
            target
                .sync_all()
                .map_err(|error| AttemptFailure::Other(error.into()))?;
        }
    } else {
        let length = fs::metadata(&part).map(|value| value.len()).unwrap_or(0);
        if length > total {
            return Err(AttemptFailure::Other(anyhow!(
                "Eski kısmi dosya beklenen boyutu aşıyor; eser korundu"
            )));
        }
        if length > 0 {
            OpenOptions::new()
                .write(true)
                .open(&part)
                .and_then(|file| file.sync_all())
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            completed.push(ByteRange {
                start: 0,
                end: length,
            });
        }
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part)
            .and_then(|file| {
                mark_sparse(&file);
                file.set_len(total)?;
                file.sync_all()
            })
            .map_err(|error| AttemptFailure::Other(error.into()))?;
    }
    let state = ResumeState {
        version: STATE_VERSION,
        url: source_url_digest(&remote.url),
        etag: remote.etag.clone(),
        last_modified: remote.last_modified.clone(),
        total: Some(total),
        segmented: true,
        connections,
        checksum,
        completed: normalize_completed(&completed, total)?,
    };
    write_state(&state_path(output), &state).map_err(AttemptFailure::Other)?;
    cleanup_legacy_segments(output);
    Ok(state)
}

fn normalize_completed(
    ranges: &[ByteRange],
    total: u64,
) -> std::result::Result<Vec<ByteRange>, AttemptFailure> {
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<ByteRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.start >= range.end || range.end > total {
            return Err(AttemptFailure::Other(anyhow!(
                "Kalıcı aralık haritası geçersiz"
            )));
        }
        if let Some(previous) = normalized.last_mut() {
            if range.start < previous.end {
                return Err(AttemptFailure::Other(anyhow!(
                    "Kalıcı aralık haritasında çakışma var"
                )));
            }
            if range.start == previous.end {
                previous.end = range.end;
                continue;
            }
        }
        normalized.push(range);
    }
    Ok(normalized)
}

fn missing_ranges(total: u64, completed: &[ByteRange]) -> Vec<RangeWork> {
    let mut missing = Vec::with_capacity(completed.len() + 1);
    let mut offset = 0;
    for range in completed {
        if offset < range.start {
            missing.push(RangeWork {
                range: ByteRange {
                    start: offset,
                    end: range.start,
                },
                attempts: 0,
            });
        }
        offset = range.end;
    }
    if offset < total {
        missing.push(RangeWork {
            range: ByteRange {
                start: offset,
                end: total,
            },
            attempts: 0,
        });
    }
    missing
}

fn range_can_split(length: u64, maximum: u64) -> bool {
    length > maximum && length - maximum >= MIN_SEGMENT_SIZE
}

fn take_largest_range(pending: &mut Vec<RangeWork>, maximum: u64) -> RangeWork {
    let index = pending
        .iter()
        .enumerate()
        .max_by_key(|(_, work)| work.range.len())
        .map(|(index, _)| index)
        .expect("pending range missing");
    if !range_can_split(pending[index].range.len(), maximum) {
        return pending.swap_remove(index);
    }
    let start = pending[index].range.start;
    pending[index].range.start += maximum;
    RangeWork {
        range: ByteRange {
            start,
            end: start + maximum,
        },
        attempts: pending[index].attempts,
    }
}

fn legacy_segments_exist(output: &Path) -> bool {
    (0..16).any(|index| segment_path(output, index).exists())
}

fn cleanup_legacy_segments(output: &Path) {
    for index in 0..16 {
        remove_if_exists(&segment_path(output, index));
    }
}

fn download_ftp(
    request: &AddRequest,
    output: &Path,
    url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let remote = probe_ftp(request, url, control, network)?;
    let part = part_path(output);
    let state_path = state_path(output);
    let previous = read_state(&state_path);
    let offset = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let may_resume = offset > 0
        && previous
            .as_ref()
            .map(|state| ftp_state_matches(state, &remote))
            .unwrap_or(false)
        && remote.last_modified.is_some()
        && remote.total.map(|total| offset < total).unwrap_or(false);
    if !may_resume && offset > 0 {
        return Err(preserved_partial(crate::i18n::ui(
"FTP kısmi dosyasının kimliği kanıtlanamadı; yeniden başlatma seçilene kadar korundu",
                "The FTP partial file's identity could not be proven; it is kept until you choose to restart",
)));
    }
    write_state(
        &state_path,
        &ResumeState {
            version: STATE_VERSION,
            url: hex::encode(Sha256::digest(remote.url.as_str().as_bytes())),
            etag: None,
            last_modified: remote.last_modified.clone(),
            total: remote.total,
            segmented: false,
            connections: 1,
            checksum: normalized_checksum(request.checksum.as_deref()),
            completed: Vec::new(),
        },
    )?;

    let mut easy = network.ftp_easy(&remote.url, control)?;
    configure_easy(&mut easy, &remote.url)?;
    easy.http_headers(build_headers(
        request,
        &remote.url,
        &origin(&remote.url),
        None,
    )?)?;
    if offset > 0 {
        easy.resume_from(offset)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&part)?;
    if offset == 0 {
        file.set_len(0)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut written = 0u64;
    let mut last = Instant::now();
    let mut last_bytes = offset;
    let mut throttle_started = Instant::now();
    let mut throttle_bytes = 0u64;
    let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
    let write_error = io_error.clone();
    easy.progress(true)?;
    let result = {
        let mut transfer = easy.transfer();
        transfer.write_function(|data| {
            if control.stop_requested() {
                return Ok(0);
            }
            if let Err(error) = file.write_all(data) {
                *write_error.borrow_mut() = Some(error);
                return Ok(0);
            }
            written += data.len() as u64;
            throttle_bytes += data.len() as u64;
            throttle(control, 1, &mut throttle_started, &mut throttle_bytes);
            let now = Instant::now();
            if now.duration_since(last) >= Duration::from_millis(200) {
                let current = offset + written;
                let elapsed = now.duration_since(last).as_secs_f64().max(0.001);
                let speed = ((current - last_bytes) as f64 / elapsed) as u64;
                progress(TransferProgress {
                    downloaded: current,
                    total: remote.total,
                    speed,
                    eta: eta(remote.total, current, speed),
                    phase: crate::i18n::ui("FTP indiriliyor", "Downloading over FTP").into(),
                });
                last = now;
                last_bytes = current;
            }
            Ok(data.len())
        })?;
        transfer.progress_function(|_, _, _, _| !control.stop_requested())?;
        transfer.perform()
    };
    if control.stop_requested() {
        file.sync_all().context(crate::i18n::ui(
            "Duraklatılan FTP dosyası diske yazılamadı",
            "The paused FTP file could not be written to disk",
        ))?;
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        );
    }
    if let Some(error) = io_error.borrow_mut().take() {
        return Err(anyhow!(error).context(crate::i18n::ui(
            "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
            "The file could not be written; the disk is full or access was denied",
        )));
    }
    if let Err(error) = result {
        return Err(governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "FTP aktarımı tamamlanamadı",
                "The FTP transfer did not complete",
            ),
        ));
    }
    file.flush()?;
    let bytes = offset + written;
    if let Some(total) = remote.total {
        if bytes != total {
            bail!(
                "{}",
                crate::i18n::ui_owned!(
                    format!("FTP aktarımı eksik kaldı: {bytes}/{total} bayt"),
                    format!("FTP transfer incomplete: {bytes}/{total} bytes"),
                )
            );
        }
    }
    finalize_part(
        output,
        &part,
        &state_path,
        bytes,
        control,
        request.checksum.as_deref(),
    )
}

fn probe_http(
    request: &AddRequest,
    mut url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<RemoteInfo> {
    let original_origin = origin(&url);
    let mut use_get = false;
    let mut redirects = 0;
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let mut easy = network.easy_for_url(&url, control)?;
        configure_easy(&mut easy, &url)?;
        easy.nobody(!use_get)?;
        if use_get {
            easy.range("0-0")?;
        }
        easy.progress(true)?;
        easy.timeout(Duration::from_secs(45))?;
        easy.follow_location(false)?;
        easy.http_headers(build_headers(request, &url, &original_origin, None)?)?;
        let response = Rc::new(RefCell::new(ResponseHeaders::default()));
        let callback = response.clone();
        let result = {
            let mut transfer = easy.transfer();
            transfer.header_function(move |line| {
                parse_header_line(&mut callback.borrow_mut(), line);
                true
            })?;
            // Metadata only: intentionally abort at the first body chunk even if Range is ignored.
            transfer.write_function(|_| Ok(0))?;
            transfer.progress_function(|_, _, _, _| !control.stop_requested())?;
            transfer.perform()
        };
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let headers = response.borrow().clone();
        if is_redirect(headers.status) {
            if redirects == MAX_REDIRECTS {
                bail!(
                    "{}",
                    crate::i18n::ui("Çok fazla HTTP yönlendirmesi", "Too many HTTP redirects",)
                );
            }
            url = redirected_url(&url, headers.location.as_deref())?;
            redirects += 1;
            continue;
        }
        if !use_get && matches!(headers.status, 403 | 405 | 501) {
            use_get = true;
            continue;
        }
        if let Err(error) = result {
            if !(use_get && error.is_write_error() && headers.headers_complete) {
                return Err(governed_transport_error(
                    &easy,
                    error,
                    crate::i18n::ui(
                        "Sunucu bilgileri alınamadı",
                        "The server information could not be retrieved",
                    ),
                ));
            }
        }
        if matches!(headers.status, 429 | 503) {
            return Err(retryable_status_error(headers.status, headers.retry_after));
        }
        if !(200..300).contains(&headers.status) {
            return Err(anyhow!(HttpStatusError {
                status: headers.status
            }));
        }
        let total = if headers.status == 206 {
            headers.content_range.and_then(|r| r.total)
        } else {
            headers.content_length
        };
        return Ok(RemoteInfo {
            url,
            credential_origin: original_origin,
            total,
            etag: headers.etag,
            last_modified: headers.last_modified,
            accept_ranges: headers.accept_ranges || headers.status == 206,
            content_type: headers.content_type,
            filename: headers.filename,
        });
    }
}

fn probe_ftp(
    request: &AddRequest,
    url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<RemoteInfo> {
    let mut easy = network.ftp_easy(&url, control)?;
    configure_easy(&mut easy, &url)?;
    easy.nobody(true)?;
    easy.progress(true)?;
    easy.timeout(Duration::from_secs(45))?;
    let signal = control.clone();
    easy.progress_function(move |_, _, _, _| !signal.stop_requested())?;
    easy.fetch_filetime(true)?;
    easy.http_headers(build_headers(request, &url, &origin(&url), None)?)?;
    if let Err(error) = easy.perform() {
        return Err(governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "FTP sunucu bilgileri alınamadı",
                "The FTP server information could not be retrieved",
            ),
        ));
    }
    let length = easy
        .content_length_download()
        .ok()
        .filter(|v| *v >= 0.0)
        .map(|v| v as u64);
    let modified = easy.filetime().ok().flatten().map(|v| v.to_string());
    let credential_origin = origin(&url);
    Ok(RemoteInfo {
        url,
        credential_origin,
        total: length,
        etag: None,
        last_modified: modified,
        accept_ranges: false,
        content_type: None,
        filename: None,
    })
}

fn verify_range_support(
    request: &AddRequest,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> std::result::Result<bool, AttemptFailure> {
    let mut easy = network
        .easy_for_url(&remote.url, control)
        .map_err(AttemptFailure::Other)?;
    configure_easy(&mut easy, &remote.url)?;
    easy.range("0-0").map_err(anyhow::Error::from)?;
    easy.progress(true).map_err(anyhow::Error::from)?;
    easy.timeout(Duration::from_secs(30))
        .map_err(anyhow::Error::from)?;
    easy.follow_location(false).map_err(anyhow::Error::from)?;
    easy.http_headers(build_headers(
        request,
        &remote.url,
        &remote.credential_origin,
        range_validator(remote),
    )?)
    .map_err(anyhow::Error::from)?;
    let response = Rc::new(RefCell::new(ResponseHeaders::default()));
    let header_callback = response.clone();
    let body_response = response.clone();
    let expected = ExpectedResponse::Range {
        start: 0,
        end: Some(0),
        total: remote.total,
    };
    let result = {
        let mut transfer = easy.transfer();
        transfer
            .progress_function(|_, _, _, _| !control.stop_requested())
            .map_err(anyhow::Error::from)?;
        transfer
            .header_function(move |line| {
                parse_header_line(&mut header_callback.borrow_mut(), line);
                true
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .write_function(move |data| {
                if response_allows_remote_body(&body_response.borrow(), expected, remote) {
                    Ok(data.len())
                } else {
                    Ok(0)
                }
            })
            .map_err(anyhow::Error::from)?;
        transfer.perform()
    };
    let headers = response.borrow().clone();
    if validators_changed(&headers, remote) {
        return Err(AttemptFailure::ResourceChanged);
    }
    Ok(result.is_ok() && response_allows_body(&headers, expected))
}

fn configure_easy(easy: &mut GovernedEasy, url: &Url) -> Result<()> {
    easy.url(url.as_str())?;
    easy.useragent(concat!("SSDownload/", env!("CARGO_PKG_VERSION")))?;
    easy.connect_timeout(Duration::from_secs(20))?;
    easy.low_speed_limit(1)?;
    easy.low_speed_time(Duration::from_secs(45))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    easy.fail_on_error(false)?;
    easy.follow_location(false)?;
    // Site logins answer only for their own host (see `proxy::site_login`); every
    // redirect hop configures a new handle for its own URL, so credentials never follow.
    if let Some((username, password)) = crate::proxy::site_login(url) {
        let mut auth = curl::easy::Auth::new();
        auth.basic(true);
        easy.http_auth(&auth)?;
        easy.username(&username)?;
        easy.password(&password)?;
    }
    Ok(())
}

fn governed_transport_error(
    easy: &GovernedEasy,
    error: curl::Error,
    context: &'static str,
) -> anyhow::Error {
    let error = anyhow!(error).context(context);
    match easy.network_error() {
        Some(failure) => error.context(failure),
        None => error,
    }
}

fn build_headers(
    request: &AddRequest,
    current: &Url,
    original_origin: &(String, String, u16),
    if_range: Option<&str>,
) -> Result<List> {
    let same_origin = &origin(current) == original_origin;
    let mut list = List::new();
    for (name, value) in &request.headers {
        let lower = name.trim().to_ascii_lowercase();
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            bail!(
                "{}",
                crate::i18n::ui("Geçersiz HTTP başlığı", "Invalid HTTP header",)
            );
        }
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "range" | "if-range"
        ) {
            continue;
        }
        if !header_is_safe_across_origin(&lower, same_origin) {
            continue;
        }
        list.append(&format!("{}: {}", name.trim(), value))?;
    }
    if same_origin {
        if let Some(referer) = request.referer.as_deref() {
            if !referer.contains(['\r', '\n']) {
                list.append(&format!("Referer: {referer}"))?;
            }
        }
    }
    if let Some(validator) = if_range {
        list.append(&format!("If-Range: {validator}"))?;
    }
    Ok(list)
}

/// Credentials and site-specific request metadata must never follow a redirect
/// to another origin.  The short allow-list contains only browser-neutral
/// negotiation headers.
fn header_is_safe_across_origin(name: &str, same_origin: bool) -> bool {
    same_origin || matches!(name, "accept" | "accept-language" | "user-agent")
}

fn parse_header_line(headers: &mut ResponseHeaders, bytes: &[u8]) {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return;
    };
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.starts_with("HTTP/") {
        *headers = ResponseHeaders::default();
        headers.status = trimmed
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        return;
    }
    if trimmed.is_empty() {
        headers.headers_complete = true;
        return;
    }
    let Some((name, value)) = trimmed.split_once(':') else {
        return;
    };
    let value = value.trim();
    match name.trim().to_ascii_lowercase().as_str() {
        "location" => headers.location = Some(value.into()),
        "etag" => headers.etag = Some(value.into()),
        "last-modified" => headers.last_modified = Some(value.into()),
        "content-disposition" => headers.filename = disposition_name(value),
        "content-type" => {
            headers.content_type = Some(
                value
                    .split(';')
                    .next()
                    .unwrap_or(value)
                    .trim()
                    .to_ascii_lowercase(),
            )
        }
        "content-length" => headers.content_length = value.parse().ok(),
        "content-range" => headers.content_range = parse_content_range(value),
        "accept-ranges" => headers.accept_ranges = value.eq_ignore_ascii_case("bytes"),
        "retry-after" => headers.retry_after = parse_retry_after(value),
        _ => {}
    }
}

/// Longest server-requested pause honoured before a retry; a larger `Retry-After` is
/// clamped so a server cannot park a job indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

fn parse_retry_after(value: &str) -> Option<Duration> {
    let delay = if let Ok(seconds) = value.trim().parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        let deadline = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
        let seconds = deadline
            .timestamp()
            .saturating_sub(chrono::Utc::now().timestamp());
        Duration::from_secs(seconds.max(0) as u64)
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

fn range_retry_delay(attempt: u8) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6))
}

fn wait_for_retry(control: &TransferControl, delay: Duration) -> Result<()> {
    let deadline = Instant::now().checked_add(delay).context(crate::i18n::ui(
        "Retry-After süresi desteklenen zaman aralığını aşıyor",
        "The Retry-After delay exceeds the supported time range",
    ))?;
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let value = value.trim();
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        total.parse().ok()
    };
    if total.map(|length| end >= length).unwrap_or(false) {
        return None;
    }
    Some(ContentRange { start, end, total })
}

fn response_allows_body(headers: &ResponseHeaders, expected: ExpectedResponse) -> bool {
    if !headers.headers_complete {
        return false;
    }
    match expected {
        ExpectedResponse::Full => headers.status == 200,
        ExpectedResponse::Range { start, end, total } => {
            if headers.status != 206 {
                return false;
            }
            let Some(range) = headers.content_range else {
                return false;
            };
            range.start == start
                && end
                    .map(|expected_end| range.end == expected_end)
                    .unwrap_or(range.end >= start)
                && total
                    .map(|expected_total| range.total == Some(expected_total))
                    .unwrap_or(true)
        }
    }
}

fn response_allows_remote_body(
    headers: &ResponseHeaders,
    expected: ExpectedResponse,
    remote: &RemoteInfo,
) -> bool {
    response_allows_body(headers, expected) && !validators_changed(headers, remote)
}

/// A missing validator neither proves nor disproves remote identity. Only two
/// present validators that disagree are a resource change.
fn validators_changed(headers: &ResponseHeaders, remote: &RemoteInfo) -> bool {
    remote
        .etag
        .as_ref()
        .zip(headers.etag.as_ref())
        .map(|(a, b)| a != b)
        .unwrap_or(false)
        || remote
            .last_modified
            .as_ref()
            .zip(headers.last_modified.as_ref())
            .map(|(a, b)| a != b)
            .unwrap_or(false)
}

fn state_matches(
    state: &ResumeState,
    remote: &RemoteInfo,
    segmented: bool,
    connections: u8,
    checksum: Option<&str>,
) -> bool {
    if state.segmented != segmented || state.total != remote.total {
        return false;
    }
    if state.version == 2 {
        return state.connections == connections && legacy_state_matches(state, remote);
    }
    if state.version != STATE_VERSION {
        return false;
    }
    let strong_etag = state
        .etag
        .as_deref()
        .zip(remote.etag.as_deref())
        .and_then(|(old, new)| {
            if is_strong_etag(old) && is_strong_etag(new) {
                Some(old == new)
            } else {
                None
            }
        });
    if strong_etag == Some(false) {
        return false;
    }
    let checksum = checksum
        .zip(state.checksum.as_deref())
        .map(|(old, new)| old.eq_ignore_ascii_case(new))
        .unwrap_or(false);
    strong_etag == Some(true) || checksum || modification_identity(state, remote)
}

/// The weaker identity many servers offer: the same `Last-Modified` and the same length,
/// with no disagreeing ETag of any kind. Every range request that relies on it carries
/// `If-Range`, so a changed resource answers with a full body the range check rejects.
fn modification_identity(state: &ResumeState, remote: &RemoteInfo) -> bool {
    let etags_disagree = state
        .etag
        .as_deref()
        .zip(remote.etag.as_deref())
        .is_some_and(|(old, new)| old != new);
    !etags_disagree
        && state.total.is_some()
        && state.total == remote.total
        && state
            .last_modified
            .as_deref()
            .zip(remote.last_modified.as_deref())
            .is_some_and(|(old, new)| !old.trim().is_empty() && old == new)
}

/// Whether the remote offers any identity a resumed or segmented transfer can be bound
/// to: a strong ETag, a caller checksum, or `Last-Modified` with a known length.
fn has_resume_identity(remote: &RemoteInfo, checksum: Option<&str>) -> bool {
    remote.etag.as_deref().is_some_and(is_strong_etag)
        || normalized_checksum(checksum).is_some()
        || (remote.total.is_some()
            && remote
                .last_modified
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()))
}

fn legacy_state_matches(state: &ResumeState, remote: &RemoteInfo) -> bool {
    state.version == 2
        && state.url == source_url_digest(&remote.url)
        && state.total == remote.total
        && state
            .etag
            .as_deref()
            .zip(remote.etag.as_deref())
            .map(|(old, new)| is_strong_etag(old) && is_strong_etag(new) && old == new)
            .unwrap_or(false)
}

fn ftp_state_matches(state: &ResumeState, remote: &RemoteInfo) -> bool {
    (state.version == 2 || state.version == STATE_VERSION)
        && state.url == source_url_digest(&remote.url)
        && state.total == remote.total
        && state.last_modified == remote.last_modified
        && !state.segmented
}

fn source_url_digest(url: &Url) -> String {
    hex::encode(Sha256::digest(url.as_str().as_bytes()))
}

fn normalized_checksum(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let value = value.strip_prefix("sha256:").unwrap_or(value).trim();
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn split_ranges(total: u64, count: u8) -> Vec<(u64, u64)> {
    let count = count.max(1) as u64;
    let base = total / count;
    let remainder = total % count;
    let mut ranges = Vec::with_capacity(count as usize);
    let mut start = 0u64;
    for index in 0..count {
        let length = base + u64::from(index < remainder);
        if length == 0 {
            continue;
        }
        ranges.push((start, start + length - 1));
        start += length;
    }
    ranges
}

fn read_state(path: &Path) -> Option<ResumeState> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_state(path: &Path, state: &ResumeState) -> Result<()> {
    crate::recovery::atomic_write(path, &serde_json::to_vec(state)?)
}

fn finalize_part(
    output: &Path,
    part: &Path,
    state: &Path,
    bytes: u64,
    control: &TransferControl,
    checksum: Option<&str>,
) -> Result<TransferResult> {
    verify_checksum(part, checksum, control)?;
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        )
    }
    OpenOptions::new().write(true).open(part)?.sync_all()?;
    let actual = crate::output::publish(part, output)?;
    remove_if_exists(state);
    Ok(TransferResult {
        path: actual,
        bytes,
    })
}

fn verify_checksum(path: &Path, checksum: Option<&str>, control: &TransferControl) -> Result<()> {
    let Some(value) = checksum.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let expected = value.strip_prefix("sha256:").unwrap_or(value).trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "{}",
            crate::i18n::ui(
                "SHA-256 sağlama toplamı 64 onaltılık karakter olmalıdır",
                "The SHA-256 checksum must be 64 hexadecimal characters",
            )
        );
    }
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER];
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("Doğrulama durduruldu", "Verification stopped",)
            )
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = hex::encode(digest.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        crate::bail_code!(crate::error_codes::TRF_003, "{}", crate::i18n::ui("SHA-256 doğrulaması başarısız oldu; kısmi eser inceleme veya açık yeniden başlatma için korundu", "SHA-256 verification failed; the partial artifact was kept for inspection or to resume after a restart"));
    }
    Ok(())
}

fn part_path(output: &Path) -> PathBuf {
    append_suffix(output, ".ssdownload.part")
}
fn state_path(output: &Path) -> PathBuf {
    append_suffix(output, ".ssdownload.state")
}
fn segment_path(output: &Path, index: usize) -> PathBuf {
    append_suffix(output, &format!(".ssdownload.part.{index:03}"))
}
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}
fn remove_if_exists(path: &Path) {
    let _ = fs::remove_file(path);
}

fn redirected_url(current: &Url, location: Option<&str>) -> Result<Url> {
    let location = location.ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui("Yönlendirme hedefi eksik", "The redirect target is missing")
        )
    })?;
    let next = current.join(location).context(crate::i18n::ui(
        "Yönlendirme hedefi geçersiz",
        "The redirect target is invalid",
    ))?;
    if current.scheme() == "https" && next.scheme() != "https" {
        bail!(
            "{}",
            crate::i18n::ui(
                "HTTPS bağlantısının şifresiz bir adrese yönlendirilmesi reddedildi.",
                "Redirecting an HTTPS connection to an insecure address was rejected.",
            )
        );
    }
    match next.scheme() {
        "http" | "https" => {}
        _ => bail!(
            "{}",
            crate::i18n::ui(
                "Güvenli olmayan yönlendirme reddedildi",
                "Unsafe redirect rejected",
            )
        ),
    }
    if next.host_str().is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yönlendirme sunucusu eksik",
                "The redirect address has no host name",
            )
        );
    }
    Ok(next)
}

fn origin(url: &Url) -> (String, String, u16) {
    (
        url.scheme().to_ascii_lowercase(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or(0),
    )
}
fn is_redirect(status: u32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}
fn is_strong_etag(value: &str) -> bool {
    !value.trim_start().starts_with("W/")
}
fn range_validator(remote: &RemoteInfo) -> Option<&str> {
    remote
        .etag
        .as_deref()
        .filter(|etag| is_strong_etag(etag))
        .or(remote.last_modified.as_deref())
}
pub(crate) fn request_requires_media_pipeline(request: &AddRequest) -> bool {
    request.format_id.is_some()
        || request.video_format_id.is_some()
        || request.container.is_some()
        || request.max_height.is_some()
        || request.exact_height.is_some()
        || request.audio_format_id.is_some()
        || request.audio_language.is_some()
        || !request.audio_tracks.is_empty()
        || request.subtitle_mode.is_some()
        || !request.external_subtitles.is_empty()
        || !request.subtitle_languages.is_empty()
        || !request.subtitle_tracks.is_empty()
        || request.audio_format.is_some()
        || request.expected_audio.is_some()
        || request.playlist
}

fn media_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}
fn is_media_content_type(value: &str) -> bool {
    matches!(
        value,
        "text/html"
            | "application/xhtml+xml"
            | "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "application/dash+xml"
    )
}
fn eta(total: Option<u64>, downloaded: u64, speed: u64) -> Option<u64> {
    total
        .filter(|total| *total >= downloaded)
        .and_then(|total| (total - downloaded).checked_div(speed))
}
fn throttle(control: &TransferControl, divisor: u64, started: &mut Instant, bytes: &mut u64) {
    loop {
        let limit = control.speed_limit();
        if limit == 0 || control.stop_requested() {
            *started = Instant::now();
            *bytes = 0;
            return;
        }
        let share = (limit / divisor.max(1)).max(1);
        let expected = Duration::from_secs_f64(*bytes as f64 / share as f64);
        let elapsed = started.elapsed();
        if expected <= elapsed {
            break;
        }
        thread::sleep((expected - elapsed).min(Duration::from_millis(50)));
    }
    if started.elapsed() >= Duration::from_secs(1) {
        *started = Instant::now();
        *bytes = 0;
    }
}

fn final_name(
    output: &Path,
    request: &AddRequest,
    remote: &RemoteInfo,
    response: Option<&ResponseHeaders>,
) -> PathBuf {
    if request.filename.is_none() {
        if let Some(name) = response
            .and_then(|headers| headers.filename.as_ref())
            .or(remote.filename.as_ref())
        {
            return output.with_file_name(crate::engine::safe_filename(name, "download"));
        }
    }
    output.to_owned()
}
fn disposition_name(value: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ';' && !quoted {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    let mut plain = None;
    for part in parts.into_iter().skip(1) {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("filename*") {
            let Some((charset, tail)) = value.split_once('\'') else {
                continue;
            };
            let Some((_, encoded)) = tail.split_once('\'') else {
                continue;
            };
            if !charset.eq_ignore_ascii_case("utf-8") {
                continue;
            }
            let input = encoded.as_bytes();
            let mut bytes = Vec::new();
            let mut i = 0;
            let mut valid = true;
            while i < input.len() {
                if input[i] == b'%' {
                    if i + 2 < input.len() {
                        if let (Some(a), Some(b)) = (
                            (input[i + 1] as char).to_digit(16),
                            (input[i + 2] as char).to_digit(16),
                        ) {
                            bytes.push((a * 16 + b) as u8);
                            i += 3;
                            continue;
                        }
                    }
                    valid = false;
                    break;
                }
                bytes.push(input[i]);
                i += 1;
            }
            if valid {
                if let Ok(name) = String::from_utf8(bytes) {
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
        } else if key.eq_ignore_ascii_case("filename") {
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            let mut name = String::new();
            let mut escape = false;
            for ch in value.chars() {
                if !escape && ch == '\\' {
                    escape = true;
                    continue;
                }
                name.push(ch);
                escape = false;
            }
            if !name.is_empty() {
                plain = Some(name);
            }
        }
    }
    plain
}

/// Moves only the named legacy partial set under the job's owned scratch directory.
/// An interrupted migration is repeatable; never replace an existing scratch file.
pub(crate) fn migrate_legacy_parts(original: &Path, scratch: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let mut candidates = vec![part_path(original)];
    candidates.extend((0..16).map(|index| segment_path(original, index)));
    candidates.push(state_path(original));
    for source in candidates {
        if !source.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Eski parça dosyası güvenle taşınamıyor",
                    "The legacy partial file cannot be moved safely",
                )
            );
        }
        let target = scratch
            .parent()
            .context(crate::i18n::ui(
                "Çalışma klasörü eksik",
                "The working folder is missing",
            ))?
            .join(source.file_name().context(crate::i18n::ui(
                "Parça adı eksik",
                "The part name is missing",
            ))?);
        if target.exists() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Eski ve yeni çalışma dosyaları çakışıyor; dosyalar korundu",
                    "The legacy and new working files collide; the files were kept",
                )
            );
        }
        fs::rename(&source, &target).context(crate::i18n::ui(
            "Eski parça çalışma klasörüne taşınamadı",
            "The old part could not be moved to the working folder",
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        time::SystemTime,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssdownload-transfer-{label}-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn response(stream: &mut TcpStream, status: &str, headers: &[&str], body: &[u8]) {
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    fn local_request(url: String) -> AddRequest {
        AddRequest {
            url,
            ..Default::default()
        }
    }

    fn remote_with_etag(etag: Option<&str>) -> RemoteInfo {
        let url = validate_source_url("https://example.test/payload.bin").unwrap();
        RemoteInfo {
            credential_origin: origin(&url),
            url,
            total: Some(100),
            etag: etag.map(str::to_owned),
            last_modified: None,
            accept_ranges: true,
            content_type: None,
            filename: None,
        }
    }

    #[test]
    fn rejects_malformed_and_out_of_bounds_content_ranges() {
        assert!(parse_content_range("bytes 10-19/20").is_some());
        assert!(parse_content_range("bytes 20-10/30").is_none());
        assert!(parse_content_range("bytes 0-20/20").is_none());
        assert!(parse_content_range("items 0-1/2").is_none());
    }

    #[test]
    fn range_response_must_match_the_exact_requested_segment() {
        let mut headers = ResponseHeaders {
            status: 206,
            headers_complete: true,
            ..Default::default()
        };
        headers.content_range = parse_content_range("bytes 0-9/100");
        assert!(response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 0,
                end: Some(9),
                total: Some(100)
            }
        ));
        assert!(!response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 10,
                end: Some(19),
                total: Some(100)
            }
        ));
        headers.status = 200;
        assert!(!response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 0,
                end: Some(9),
                total: Some(100)
            }
        ));
    }

    #[test]
    fn valid_range_rejects_a_conflicting_remote_validator() {
        let remote = remote_with_etag(Some("\"fixture-v1\""));
        let mut headers = ResponseHeaders {
            status: 206,
            etag: Some("\"fixture-v2\"".into()),
            headers_complete: true,
            ..Default::default()
        };
        headers.content_range = parse_content_range("bytes 0-0/100");
        let expected = ExpectedResponse::Range {
            start: 0,
            end: Some(0),
            total: Some(100),
        };
        assert!(response_allows_body(&headers, expected));
        assert!(!response_allows_remote_body(&headers, expected, &remote));

        headers.etag = Some("\"fixture-v1\"".into());
        assert!(response_allows_remote_body(&headers, expected, &remote));
        headers.etag = None;
        assert!(response_allows_remote_body(&headers, expected, &remote));
    }

    #[test]
    fn strong_etag_conflict_cannot_be_overridden_by_checksum() {
        let checksum = "ab".repeat(32);
        let state = ResumeState {
            version: STATE_VERSION,
            url: String::new(),
            etag: Some("\"fixture-v1\"".into()),
            last_modified: None,
            total: Some(100),
            segmented: false,
            connections: 1,
            checksum: Some(checksum.clone()),
            completed: Vec::new(),
        };
        assert!(!state_matches(
            &state,
            &remote_with_etag(Some("\"fixture-v2\"")),
            false,
            1,
            Some(&checksum),
        ));
        assert!(state_matches(
            &state,
            &remote_with_etag(Some("\"fixture-v1\"")),
            false,
            1,
            Some(&checksum),
        ));
        assert!(state_matches(
            &state,
            &remote_with_etag(None),
            false,
            1,
            Some(&checksum),
        ));
        // Last-Modified plus length is an identity; a changed date or a disagreeing weak
        // ETag is not.
        let dated = ResumeState {
            etag: None,
            last_modified: Some("Wed, 01 Jan 2026 00:00:00 GMT".into()),
            checksum: None,
            ..state
        };
        let mut remote = remote_with_etag(None);
        remote.last_modified = dated.last_modified.clone();
        assert!(state_matches(&dated, &remote, false, 1, None));
        remote.last_modified = Some("Thu, 02 Jan 2026 00:00:00 GMT".into());
        assert!(!state_matches(&dated, &remote, false, 1, None));
        remote.last_modified = dated.last_modified.clone();
        let weak = ResumeState {
            etag: Some("W/\"a\"".into()),
            ..dated
        };
        remote.etag = Some("W/\"b\"".into());
        assert!(!state_matches(&weak, &remote, false, 1, None));
    }

    #[test]
    fn selected_video_and_subtitle_tracks_require_media_pipeline() {
        let mut request = AddRequest {
            video_format_id: Some("video-1".into()),
            ..Default::default()
        };
        assert!(request_requires_media_pipeline(&request));
        request.video_format_id = None;
        request.subtitle_tracks.push(Default::default());
        assert!(request_requires_media_pipeline(&request));
    }

    #[test]
    fn split_ranges_cover_each_byte_once() {
        assert_eq!(split_ranges(10, 3), vec![(0, 3), (4, 6), (7, 9)]);
        assert_eq!(split_ranges(2, 4), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn only_supported_network_schemes_are_accepted() {
        assert!(validate_source_url("https://example.test/file").is_ok());
        assert!(validate_source_url("ftp://example.test/file").is_ok());
        assert!(validate_source_url("file:///C:/secret.txt").is_err());
        assert!(validate_source_url("data:text/plain,secret").is_err());
    }

    #[test]
    fn content_disposition_prefers_utf8_and_handles_quoted_semicolons() {
        assert_eq!(
            disposition_name(
                "attachment; filename=plain.txt; filename*=UTF-8''rapor%20%C3%B6zel.txt"
            ),
            Some("rapor özel.txt".into())
        );
        assert_eq!(
            disposition_name("attachment; filename=\"one; two.txt\""),
            Some("one; two.txt".into())
        );
        assert_eq!(
            disposition_name("attachment; filename*=ISO-8859-1''bad"),
            None
        );
    }

    #[test]
    fn redirect_never_allows_site_credentials_to_cross_origins() {
        assert!(header_is_safe_across_origin("authorization", true));
        assert!(!header_is_safe_across_origin("authorization", false));
        assert!(!header_is_safe_across_origin("cookie", false));
        assert!(!header_is_safe_across_origin("x-site-token", false));
        assert!(header_is_safe_across_origin("accept", false));
        assert!(header_is_safe_across_origin("accept-language", false));
    }

    #[test]
    fn checksum_failure_preserves_the_partial_file_for_explicit_restart() {
        let directory = TestDirectory::new("checksum");
        let part = directory.0.join("payload.part");
        fs::write(&part, b"payload").unwrap();
        let control = TransferControl::default();
        let error = verify_checksum(&part, Some(&"0".repeat(64)), &control).unwrap_err();
        assert!(error.to_string().contains("doğrulaması başarısız"));
        assert!(part.exists());
    }

    #[test]
    fn source_refresh_preparation_is_cancellable_and_does_not_stage_early() {
        let directory = TestDirectory::new("refresh-stage");
        let output = directory.0.join("payload.bin");
        fs::write(part_path(&output), b"x").unwrap();
        let previous = AddRequest {
            url: "https://old.example.test/payload.bin".into(),
            ..Default::default()
        };
        let old_state = ResumeState {
            version: STATE_VERSION,
            url: source_url_digest(&validate_source_url(&previous.url).unwrap()),
            etag: Some("\"stable-v1\"".into()),
            total: Some(4),
            segmented: false,
            connections: 1,
            checksum: None,
            completed: vec![ByteRange { start: 0, end: 1 }],
            last_modified: None,
        };
        write_state(&state_path(&output), &old_state).unwrap();
        let before = fs::read(state_path(&output)).unwrap();

        let network = NetworkGovernor::new(16);
        let cancelled = TransferControl::default();
        cancelled.cancel();
        assert!(
            prepare_source_refresh(&previous, &previous, &output, false, &cancelled, &network)
                .is_err()
        );
        assert_eq!(fs::read(state_path(&output)).unwrap(), before);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("HEAD "));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"stable-v1\"\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
        });
        let next = AddRequest {
            url: format!("http://{address}/payload.bin"),
            ..Default::default()
        };
        let prepared = prepare_source_refresh(
            &previous,
            &next,
            &output,
            false,
            &TransferControl::default(),
            &network,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(state_path(&output)).unwrap(), before);
        assert!(stage_source_refresh(&prepared).unwrap());
        let staged = read_state(&state_path(&output)).unwrap();
        assert_eq!(
            staged.url,
            source_url_digest(&validate_source_url(&next.url).unwrap())
        );
    }

    #[test]
    fn head_rejection_falls_back_to_ranged_get_and_uses_server_filename() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = b"fallback-data".to_vec();
        let served = payload.clone();
        let (server_done, server_result) = mpsc::channel();
        thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                seen.push(request.clone());
                if request.starts_with("HEAD ") {
                    response(&mut stream, "405 Method Not Allowed", &[], b"");
                } else if request.contains("Range: bytes=0-0") {
                    response(
                        &mut stream,
                        "206 Partial Content",
                        &[
                            "Content-Range: bytes 0-0/13",
                            "Accept-Ranges: bytes",
                            "ETag: \"fixture-v1\"",
                        ],
                        &served[..1],
                    );
                } else {
                    response(
                        &mut stream,
                        "200 OK",
                        &[
                            "Accept-Ranges: bytes",
                            "ETag: \"fixture-v1\"",
                            "Content-Disposition: attachment; filename*=UTF-8''rapor%20%C3%B6zel.txt",
                        ],
                        &served,
                    );
                }
            }
            server_done.send(seen).unwrap();
        });

        let url = format!("http://{address}/file");
        let request = local_request(url);
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        assert!(!media);
        let directory = TestDirectory::new("head-fallback");
        let output = directory.0.join("chosen-name.bin");
        let result = download(
            &request,
            &output,
            &control,
            &network,
            1,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&result.path).unwrap(), payload);
        assert_eq!(
            result.path.file_name().unwrap().to_string_lossy(),
            "rapor özel.txt"
        );
        let seen = server_result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(seen[0].starts_with("HEAD "));
        assert!(seen[1].starts_with("GET "));
        assert!(seen[1].contains("Range: bytes=0-0"));
    }

    #[test]
    fn interrupted_http_transfer_resumes_with_an_exact_range() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..128).collect();
        let served = payload.clone();
        let (server_done, server_result) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request.clone());
                match index {
                    0 | 2 => {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\nAccept-Ranges: bytes\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        stream.flush().unwrap();
                    }
                    1 => {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\nAccept-Ranges: bytes\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                            )
                            .unwrap();
                        stream.write_all(&served[..31]).unwrap();
                        stream.flush().unwrap();
                    }
                    _ => {
                        response(
                            &mut stream,
                            "206 Partial Content",
                            &["Content-Range: bytes 31-127/128", "ETag: \"resume-v1\""],
                            &served[31..],
                        );
                    }
                }
            }
            server_done.send(requests).unwrap();
        });

        let request = local_request(format!("http://{address}/resume"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let directory = TestDirectory::new("resume");
        let output = directory.0.join("resume.bin");
        assert!(download(&request, &output, &control, &network, 1, None, &mut |_| {}).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);
        let result = download(&request, &output, &control, &network, 1, None, &mut |_| {});
        let requests = server_result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.is_ok(), "resume request sequence was {requests:#?}");
        let result = result.unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(requests[3].contains("Range: bytes=31-"));
    }

    #[test]
    fn cross_origin_redirect_strips_credentials_during_probe_and_download() {
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let (target_done, target_requests) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = target_listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request);
                response(
                    &mut stream,
                    "200 OK",
                    &["Content-Type: application/octet-stream"],
                    b"safe",
                );
            }
            target_done.send(requests).unwrap();
        });

        let source_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source_listener.local_addr().unwrap();
        let (source_done, source_request) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = source_listener.accept().unwrap();
            let request = read_request(&mut stream);
            response(
                &mut stream,
                "302 Found",
                &[&format!("Location: http://{target_address}/target")],
                b"",
            );
            source_done.send(request).unwrap();
        });

        let mut request = local_request(format!("http://{source_address}/source"));
        request
            .headers
            .insert("Authorization".into(), "Bearer secret".into());
        request
            .headers
            .insert("Cookie".into(), "sid=private".into());
        request
            .headers
            .insert("X-Site-Token".into(), "do-not-forward".into());
        request.headers.insert("Accept".into(), "*/*".into());
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (_media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        let directory = TestDirectory::new("redirect-headers");
        let result = download(
            &request,
            &directory.0.join("redirect.bin"),
            &control,
            &network,
            1,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(result.path).unwrap(), b"safe");

        let source = source_request.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(source.contains("Authorization: Bearer secret"));
        let target = target_requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        for request in target {
            let lower = request.to_ascii_lowercase();
            assert!(!lower.contains("authorization:"));
            assert!(!lower.contains("cookie:"));
            assert!(!lower.contains("x-site-token:"));
            assert!(lower.contains("accept: */*"));
        }
    }

    #[test]
    fn stable_large_resource_downloads_exactly_once_per_range_then_merges() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..(MIN_SEGMENT_SIZE * 2 + 257))
            .map(|index| (index % 251) as u8)
            .collect();
        let total = payload.len();
        let served = std::sync::Arc::new(payload.clone());
        let (server_done, server_requests) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("HEAD ") {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"segmented-v1\"\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .unwrap();
                    stream.flush().unwrap();
                } else {
                    let value = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .expect("ranged request")
                        .split_once(':')
                        .unwrap()
                        .1
                        .trim()
                        .strip_prefix("bytes=")
                        .unwrap();
                    let (start, end) = value.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end: usize = if end.is_empty() {
                        total - 1
                    } else {
                        end.parse().unwrap()
                    };
                    assert!(start <= end && end < total);
                    response(
                        &mut stream,
                        "206 Partial Content",
                        &[
                            &format!("Content-Range: bytes {start}-{end}/{total}"),
                            "Accept-Ranges: bytes",
                            "ETag: \"segmented-v1\"",
                        ],
                        &served[start..=end],
                    );
                }
                requests.push(request);
            }
            server_done.send(requests).unwrap();
        });

        let request = local_request(format!("http://{address}/large"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (_media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        let directory = TestDirectory::new("segmented");
        let output = directory.0.join("large.bin");
        let result = download(
            &request,
            &output,
            &control,
            &network,
            2,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists());
        assert!(!segment_path(&output, 0).exists());
        assert!(!segment_path(&output, 1).exists());
        let requests = server_requests
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("HEAD "))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("Range:"))
                .count(),
            3
        );
    }

    #[test]
    fn cancellation_stops_an_in_progress_http_download_and_preserves_the_part() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = vec![0x5a; 512 * 1024];
        thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let _request = read_request(&mut stream);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 524288\r\nAccept-Ranges: bytes\r\nETag: \"cancel-v1\"\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                if index == 0 {
                    continue;
                }
                for chunk in payload.chunks(16 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });

        let request = local_request(format!("http://{address}/cancel"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let cancel = control.clone();
        let directory = TestDirectory::new("cancel");
        let output = directory.0.join("cancel.bin");
        let error = download(
            &request,
            &output,
            &control,
            &network,
            1,
            None,
            &mut |progress| {
                if progress.downloaded > 0 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("durduruldu"));
        assert!(fs::metadata(part_path(&output)).unwrap().len() > 0);
        assert!(!output.exists());
    }
}
