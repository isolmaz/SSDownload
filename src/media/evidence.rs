//! Request fingerprints and pinned source evidence: manifests, DASH/HLS pinning and source state.

use super::*;

#[derive(Serialize)]
pub(super) struct ExternalSubtitleFingerprint<'a> {
    pub(super) url: &'a str,
    pub(super) language: &'a str,
    pub(super) label: &'a str,
    pub(super) kind: &'a str,
    pub(super) is_default: bool,
}

#[derive(Serialize)]
pub(super) struct MediaRequestFingerprint<'a> {
    pub(super) source: Option<MediaSourceFingerprint<'a>>,
    pub(super) output: MediaOutputFingerprint<'a>,
    pub(super) audio: MediaAudioFingerprint<'a>,
    pub(super) subtitles: MediaSubtitleFingerprint<'a>,
    pub(super) expected_audio: Option<bool>,
    pub(super) playlist: bool,
}

#[derive(Serialize)]
pub(super) struct MediaSourceFingerprint<'a> {
    pub(super) url: &'a str,
    pub(super) request_id: &'a Option<String>,
}

#[derive(Serialize)]
pub(super) struct MediaOutputFingerprint<'a> {
    pub(super) kind: DownloadKind,
    pub(super) container: &'a Option<String>,
    pub(super) max_height: Option<u32>,
    pub(super) exact_height: Option<u32>,
    pub(super) format_id: &'a Option<String>,
    pub(super) video_format_id: &'a Option<String>,
}

#[derive(Serialize)]
pub(super) struct MediaAudioFingerprint<'a> {
    pub(super) output_format: &'a Option<String>,
    pub(super) format_id: &'a Option<String>,
    pub(super) language: &'a Option<String>,
    pub(super) tracks: &'a [AudioSelection],
}

#[derive(Serialize)]
pub(super) struct MediaSubtitleFingerprint<'a> {
    pub(super) languages: &'a [String],
    pub(super) mode: &'a Option<String>,
    pub(super) tracks: &'a [SubtitleSelection],
    pub(super) external: Vec<ExternalSubtitleFingerprint<'a>>,
}

pub(super) fn fingerprint_value(
    request: &AddRequest,
    include_source: bool,
) -> MediaRequestFingerprint<'_> {
    MediaRequestFingerprint {
        source: include_source.then(|| MediaSourceFingerprint {
            url: &request.url,
            request_id: &request.request_id,
        }),
        output: MediaOutputFingerprint {
            kind: request.kind,
            container: &request.container,
            max_height: request.max_height,
            exact_height: request.exact_height,
            format_id: &request.format_id,
            video_format_id: &request.video_format_id,
        },
        audio: MediaAudioFingerprint {
            output_format: &request.audio_format,
            format_id: &request.audio_format_id,
            language: &request.audio_language,
            tracks: &request.audio_tracks,
        },
        subtitles: MediaSubtitleFingerprint {
            languages: &request.subtitle_languages,
            mode: &request.subtitle_mode,
            tracks: &request.subtitle_tracks,
            external: request
                .external_subtitles
                .iter()
                .map(|subtitle| ExternalSubtitleFingerprint {
                    url: &subtitle.url,
                    language: &subtitle.language,
                    label: &subtitle.label,
                    kind: &subtitle.kind,
                    is_default: subtitle.is_default,
                })
                .collect(),
        },
        expected_audio: request.expected_audio,
        playlist: request.playlist,
    }
}

pub(super) fn request_fingerprint(request: &AddRequest) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(
        &fingerprint_value(request, true),
    )?)))
}

pub(super) fn selection_fingerprint(request: &AddRequest) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(
        &fingerprint_value(request, false),
    )?)))
}

pub(super) const MEDIA_SOURCE_STATE_VERSION: u32 = 3;
pub(super) const MEDIA_SOURCE_FILE: &str = ".source";
pub(super) const MEDIA_SOURCE_PENDING_FILE: &str = ".source.pending";
pub(super) const MEDIA_PINNED_FILE: &str = ".source-pinned";
pub(super) const MEDIA_PINNED_PENDING_FILE: &str = ".source-pinned.pending";
pub(super) const PINNED_URL_PREFIX: &str = "ssdownload-pinned-manifest:";
pub(super) const SOURCE_EVIDENCE_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const SOURCE_EVIDENCE_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
pub(super) const MAX_SOURCE_EVIDENCE_ITEMS: usize = 16;
pub(super) const MAX_SOURCE_EVIDENCE_REQUESTS: usize = 8;
pub(super) const MAX_SOURCE_EVIDENCE_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_PINNED_SOURCE_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ManifestEvidence {
    pub(super) resolution_base_sha256: String,
    pub(super) body_sha256: String,
    pub(super) bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct DashResourceEvidence {
    pub(super) sequence: usize,
    pub(super) resource_sha256: String,
    pub(super) initialization: Option<bool>,
    pub(super) duration_millis: Option<u64>,
    pub(super) byte_range: Option<String>,
    pub(super) context_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum StreamGraphEvidence {
    Hls {
        manifest: ManifestEvidence,
    },
    Dash {
        resources: Vec<DashResourceEvidence>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct StreamEvidence {
    pub(super) format_id: String,
    pub(super) protocol: String,
    pub(super) extension: Option<String>,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
    pub(super) video_codec: Option<String>,
    pub(super) audio_codec: Option<String>,
    pub(super) language: Option<String>,
    pub(super) http_range: Option<String>,
    pub(super) graph: StreamGraphEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct SubtitleSourceEvidence {
    pub(super) language: String,
    pub(super) automatic: bool,
    pub(super) sources_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MediaItemEvidence {
    pub(super) id_sha256: Option<String>,
    pub(super) duration_millis: Option<u64>,
    pub(super) streams: Vec<StreamEvidence>,
    pub(super) subtitles: Vec<SubtitleSourceEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct MediaSourceEvidence {
    pub(super) selection: String,
    pub(super) request_context_sha256: String,
    pub(super) live: bool,
    pub(super) items: Vec<MediaItemEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MediaSourceState {
    pub(super) version: u32,
    pub(super) request: String,
    pub(super) evidence: MediaSourceEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PendingMediaSourceState {
    pub(super) version: u32,
    pub(super) previous_request: String,
    pub(super) next: MediaSourceState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PinnedManifest {
    pub(super) marker: String,
    pub(super) body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PinnedDashFormat {
    pub(super) entry_index: Option<usize>,
    pub(super) format_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PinnedMediaSource {
    pub(super) inspection: Value,
    pub(super) manifests: Vec<PinnedManifest>,
    pub(super) dash_formats: Vec<PinnedDashFormat>,
}

pub(super) fn pinned_hls_duration_tolerance(source: &PinnedMediaSource) -> Option<f64> {
    let mut largest = 0.0_f64;
    for manifest in &source.manifests {
        let mut finite = false;
        let mut has_fragments = false;
        for line in manifest.body.lines().map(str::trim) {
            if line == "#EXT-X-ENDLIST" {
                finite = true;
            }
            if let Some(value) = line.strip_prefix("#EXTINF:") {
                let duration = value.split(',').next()?.trim().parse::<f64>().ok()?;
                if !duration.is_finite() || duration <= 0.0 {
                    return None;
                }
                largest = largest.max(duration);
                has_fragments = true;
            }
        }
        if !finite || !has_fragments {
            return None;
        }
    }
    (largest > 0.0).then(|| (largest + 0.25).clamp(0.25, 10.0))
}

pub(super) struct MediaSourceCapture {
    pub(super) evidence: Option<MediaSourceEvidence>,
    pub(super) pinned: Option<PinnedMediaSource>,
    pub(super) fallback_inspection: Option<Value>,
}

impl MediaSourceCapture {
    pub(super) fn unavailable(fallback_inspection: Option<Value>) -> Self {
        Self {
            evidence: None,
            pinned: None,
            fallback_inspection,
        }
    }
}

pub(super) struct EvidenceBudget {
    pub(super) deadline: Instant,
    pub(super) requests: usize,
    pub(super) bytes: usize,
}

impl EvidenceBudget {
    pub(super) fn new() -> Self {
        Self {
            deadline: Instant::now() + SOURCE_EVIDENCE_TIMEOUT,
            requests: 0,
            bytes: 0,
        }
    }

    pub(super) fn request_timeout(&mut self) -> Option<Duration> {
        if self.requests >= MAX_SOURCE_EVIDENCE_REQUESTS {
            return None;
        }
        let remaining = self.deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        self.requests += 1;
        Some(remaining.min(SOURCE_EVIDENCE_REQUEST_TIMEOUT))
    }

    pub(super) fn remaining_bytes(&self) -> usize {
        MAX_SOURCE_EVIDENCE_BYTES.saturating_sub(self.bytes)
    }

    pub(super) fn account_bytes(&mut self, bytes: usize) -> bool {
        let Some(total) = self.bytes.checked_add(bytes) else {
            return false;
        };
        if total > MAX_SOURCE_EVIDENCE_BYTES || Instant::now() >= self.deadline {
            return false;
        }
        self.bytes = total;
        true
    }
}

pub(super) fn normalized_separate_stream_inspection(
    request: &AddRequest,
    inspection: &Value,
    selector: &str,
) -> Option<Value> {
    if request.kind != DownloadKind::Video {
        return None;
    }
    let roots = inspection
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_null())
                .take(MAX_SOURCE_EVIDENCE_ITEMS + 1)
                .map(|(index, entry)| (Some(index), entry))
                .collect::<Vec<_>>()
        })
        .filter(|entries| !entries.is_empty())
        .unwrap_or_else(|| vec![(None, inspection)]);
    if roots.len() > MAX_SOURCE_EVIDENCE_ITEMS {
        return None;
    }

    let mut selections = Vec::new();
    for (entry_index, root) in roots {
        let item_info = media_info_from_json(root).ok()?;
        let selected_ids = selected_format_ids(selector, &item_info).ok()?;
        if selected_ids.len() > 1 {
            selections.push((entry_index, selected_ids));
        }
    }
    if selections.is_empty() {
        return None;
    }

    let mut normalized = inspection.clone();
    for (entry_index, selected_ids) in selections {
        for (stream_index, id) in selected_ids.iter().enumerate() {
            if !normalize_selected_stream_role(&mut normalized, entry_index, id, stream_index == 0)
            {
                return None;
            }
        }
    }
    Some(normalized)
}

pub(super) struct ManifestCapture {
    pub(super) evidence: ManifestEvidence,
    pub(super) body: String,
}

pub(super) fn capture_media_source(
    request: &AddRequest,
    inspection: &Value,
    _info: &MediaInfo,
    selector: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<MediaSourceCapture> {
    let unavailable = || {
        MediaSourceCapture::unavailable(normalized_separate_stream_inspection(
            request, inspection, selector,
        ))
    };
    let roots = inspection
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.is_null())
                .take(MAX_SOURCE_EVIDENCE_ITEMS + 1)
                .map(|(index, entry)| (Some(index), entry.clone()))
                .collect::<Vec<_>>()
        })
        .filter(|entries| !entries.is_empty())
        .unwrap_or_else(|| vec![(None, inspection.clone())]);
    if roots.len() > MAX_SOURCE_EVIDENCE_ITEMS {
        return Ok(unavailable());
    }

    let mut budget = EvidenceBudget::new();
    let mut pinned_inspection = inspection.clone();
    let mut manifests = Vec::new();
    let mut pinned_dash_formats = Vec::new();
    let mut items = Vec::with_capacity(roots.len());
    let mut streaming = false;
    for (entry_index, root) in roots {
        if control.stop_requested() {
            bail!("Manifest kaynak kanıtı alınırken işlem durduruldu");
        }
        let Ok(item_info) = media_info_from_json(&root) else {
            return Ok(unavailable());
        };
        // yt-dlp receives one --format expression for the whole invocation. Resolve
        // that exact expression against each playlist item instead of choosing a
        // fresh per-item selector that the downloader never receives.
        let Ok(selected_ids) = selected_format_ids(selector, &item_info) else {
            return Ok(unavailable());
        };
        let Some(formats) = root.get("formats").and_then(Value::as_array) else {
            return Ok(unavailable());
        };
        let separate_streams = request.kind == DownloadKind::Video && selected_ids.len() > 1;
        let mut selected = Vec::with_capacity(selected_ids.len());
        for (stream_index, id) in selected_ids.into_iter().enumerate() {
            let Some(format) = formats.iter().find(|format| {
                format.get("format_id").and_then(Value::as_str) == Some(id.as_str())
            }) else {
                return Ok(unavailable());
            };
            let protocol = format
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let manifest_stream = protocol.contains("m3u8")
                || protocol.contains("dash")
                || format
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|value| Url::parse(value).ok())
                    .and_then(|url| url.path().rsplit('.').next().map(str::to_ascii_lowercase))
                    .is_some_and(|extension| matches!(extension.as_str(), "m3u8" | "mpd"));
            if !manifest_stream {
                continue;
            }
            streaming = true;
            if source_declares_live(&root, format, &protocol) {
                return Ok(unavailable());
            }
            if separate_streams
                && !normalize_selected_stream_role(
                    &mut pinned_inspection,
                    entry_index,
                    &id,
                    stream_index == 0,
                )
            {
                return Ok(unavailable());
            }
            let graph = if protocol.contains("m3u8") {
                let Some(target) = format.get("url").and_then(Value::as_str) else {
                    return Ok(unavailable());
                };
                let snapshot = match manifest_snapshot(
                    request,
                    format,
                    target,
                    &protocol,
                    control,
                    network,
                    &mut budget,
                ) {
                    Ok(Some(snapshot)) => snapshot,
                    Ok(None) => return Ok(unavailable()),
                    Err(error) if control.stop_requested() => return Err(error),
                    Err(_) => return Ok(unavailable()),
                };
                if snapshot
                    .body
                    .lines()
                    .all(|line| line.trim() != "#EXT-X-ENDLIST")
                {
                    return Ok(unavailable());
                }
                let marker = format!("{PINNED_URL_PREFIX}{}", manifests.len());
                if !pin_selected_hls_format(&mut pinned_inspection, entry_index, &id, &marker) {
                    return Ok(unavailable());
                }
                manifests.push(PinnedManifest {
                    marker,
                    body: snapshot.body,
                });
                StreamGraphEvidence::Hls {
                    manifest: snapshot.evidence,
                }
            } else if protocol.contains("dash") {
                let graph = match pin_selected_dash_format(
                    &mut pinned_inspection,
                    entry_index,
                    &id,
                    format,
                    &protocol,
                    control,
                    &mut budget,
                )? {
                    Some(graph) => graph,
                    None => return Ok(unavailable()),
                };
                pinned_dash_formats.push(PinnedDashFormat {
                    entry_index,
                    format_id: id.clone(),
                });
                graph
            } else {
                return Ok(unavailable());
            };
            selected.push(stream_evidence(format, id, protocol, graph));
        }
        if selected.is_empty() {
            continue;
        }
        items.push(MediaItemEvidence {
            id_sha256: root.get("id").and_then(Value::as_str).map(sha256_text),
            duration_millis: root
                .get("duration")
                .and_then(Value::as_f64)
                .filter(|duration| duration.is_finite() && *duration >= 0.0)
                .map(|duration| (duration * 1000.0).round() as u64),
            streams: selected,
            subtitles: selected_subtitle_evidence(request, &root),
        });
    }
    if !streaming || items.is_empty() || (manifests.is_empty() && pinned_dash_formats.is_empty()) {
        return Ok(unavailable());
    }
    let pinned = PinnedMediaSource {
        inspection: pinned_inspection,
        manifests,
        dash_formats: pinned_dash_formats,
    };
    if serde_json::to_vec(&pinned)?.len() > MAX_PINNED_SOURCE_BYTES {
        return Ok(unavailable());
    }
    Ok(MediaSourceCapture {
        evidence: Some(MediaSourceEvidence {
            selection: selection_fingerprint(request)?,
            request_context_sha256: request_context_fingerprint(request)?,
            live: false,
            items,
        }),
        pinned: Some(pinned),
        fallback_inspection: None,
    })
}

pub(super) fn stream_evidence(
    format: &Value,
    format_id: String,
    protocol: String,
    graph: StreamGraphEvidence,
) -> StreamEvidence {
    let http_range = format
        .get("http_headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers.iter().find_map(|(name, value)| {
                name.eq_ignore_ascii_case("range")
                    .then(|| value.as_str().map(str::to_string))
                    .flatten()
            })
        });
    StreamEvidence {
        format_id,
        protocol,
        extension: string_field(format, "ext"),
        width: as_u32(format.get("width")),
        height: as_u32(format.get("height")),
        video_codec: optional_codec(format, "vcodec"),
        audio_codec: optional_codec(format, "acodec"),
        language: string_field(format, "language"),
        http_range,
        graph,
    }
}

pub(super) fn request_context_fingerprint(request: &AddRequest) -> Result<String> {
    #[derive(Serialize)]
    struct CookieIdentity<'a> {
        name: &'a str,
        domain: &'a str,
        path: &'a str,
        secure: bool,
        host_only: bool,
        store_id: &'a Option<String>,
        partition_key: &'a Option<String>,
    }
    #[derive(Serialize)]
    struct Context<'a> {
        headers: &'a std::collections::BTreeMap<String, String>,
        referer: &'a Option<String>,
        cookies: Vec<CookieIdentity<'a>>,
    }
    let context = Context {
        headers: &request.headers,
        referer: &request.referer,
        cookies: request
            .session_cookies
            .iter()
            .map(|cookie| CookieIdentity {
                name: &cookie.name,
                domain: &cookie.domain,
                path: &cookie.path,
                secure: cookie.secure,
                host_only: cookie.host_only,
                store_id: &cookie.store_id,
                partition_key: &cookie.partition_key,
            })
            .collect(),
    };
    Ok(sha256_bytes(&serde_json::to_vec(&context)?))
}

pub(super) fn selected_subtitle_evidence(
    request: &AddRequest,
    root: &Value,
) -> Vec<SubtitleSourceEvidence> {
    let include_all = request
        .subtitle_languages
        .iter()
        .any(|language| language == "all" || language.contains(['*', '.']));
    let mut result = Vec::new();
    for (field, automatic) in [("subtitles", false), ("automatic_captions", true)] {
        let Some(languages) = root.get(field).and_then(Value::as_object) else {
            continue;
        };
        for (language, sources) in languages {
            let explicitly_selected = request
                .subtitle_tracks
                .iter()
                .any(|track| track.language == *language && track.automatic == automatic)
                || request.subtitle_languages.iter().any(|selected| {
                    selected == language
                        && request
                            .subtitle_mode
                            .as_deref()
                            .is_none_or(|mode| (mode == "automatic") == automatic)
                });
            if include_all || explicitly_selected {
                result.push(SubtitleSourceEvidence {
                    language: language.clone(),
                    automatic,
                    sources_sha256: sha256_bytes(&serde_json::to_vec(sources).unwrap_or_default()),
                });
            }
        }
    }
    result.sort_by(|left, right| {
        (&left.language, left.automatic).cmp(&(&right.language, right.automatic))
    });
    result
}

pub(super) fn selected_format<'a>(
    inspection: &'a Value,
    entry_index: Option<usize>,
    format_id: &str,
) -> Option<&'a Value> {
    let root = match entry_index {
        Some(index) => inspection
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|entries| entries.get(index)),
        None => Some(inspection),
    };
    root.and_then(|root| root.get("formats"))
        .and_then(Value::as_array)
        .and_then(|formats| {
            formats
                .iter()
                .find(|format| format.get("format_id").and_then(Value::as_str) == Some(format_id))
        })
}

pub(super) fn selected_format_mut<'a>(
    inspection: &'a mut Value,
    entry_index: Option<usize>,
    format_id: &str,
) -> Option<&'a mut Value> {
    let root = match entry_index {
        Some(index) => inspection
            .get_mut("entries")
            .and_then(Value::as_array_mut)
            .and_then(|entries| entries.get_mut(index)),
        None => Some(inspection),
    };
    root.and_then(|root| root.get_mut("formats"))
        .and_then(Value::as_array_mut)
        .and_then(|formats| {
            formats
                .iter_mut()
                .find(|format| format.get("format_id").and_then(Value::as_str) == Some(format_id))
        })
}

pub(super) fn normalize_selected_stream_role(
    inspection: &mut Value,
    entry_index: Option<usize>,
    format_id: &str,
    video: bool,
) -> bool {
    let Some(object) =
        selected_format_mut(inspection, entry_index, format_id).and_then(Value::as_object_mut)
    else {
        return false;
    };
    let (codec, extension) = if video {
        ("acodec", "audio_ext")
    } else {
        ("vcodec", "video_ext")
    };
    if object
        .get(codec)
        .and_then(Value::as_str)
        .is_some_and(|value| value != "none")
    {
        return false;
    }
    object.insert(codec.into(), Value::String("none".into()));
    object.insert(extension.into(), Value::String("none".into()));
    true
}

pub(super) fn pin_selected_hls_format(
    inspection: &mut Value,
    entry_index: Option<usize>,
    format_id: &str,
    marker: &str,
) -> bool {
    let Some(object) =
        selected_format_mut(inspection, entry_index, format_id).and_then(Value::as_object_mut)
    else {
        return false;
    };
    object.insert("url".into(), Value::String(marker.into()));
    // The extractor may have expanded a different GET into fragments before
    // our bounded verifier fetched the body. Remove that expansion so the
    // downloader must parse the exact pinned media playlist we authorized.
    object.remove("manifest_url");
    object.remove("fragments");
    object.remove("fragment_base_url");
    true
}

pub(super) fn source_declares_live(root: &Value, format: &Value, protocol: &str) -> bool {
    protocol.contains("generator")
        || [root, format]
            .into_iter()
            .any(|value| value.get("is_live").and_then(Value::as_bool) == Some(true))
        || [root, format].into_iter().any(|value| {
            matches!(
                value.get("live_status").and_then(Value::as_str),
                Some("is_live" | "is_upcoming" | "post_live")
            )
        })
}

pub(super) fn dash_fragment_url(fragment: &Value, fragment_base: Option<&Url>) -> Option<Url> {
    let value = fragment.as_object()?;
    let resolved = if let Some(url) = value.get("url").and_then(Value::as_str) {
        Url::parse(url).ok()?
    } else {
        fragment_base?.join(value.get("path")?.as_str()?).ok()?
    };
    if !matches!(resolved.scheme(), "http" | "https")
        || !resolved.username().is_empty()
        || resolved.password().is_some()
    {
        return None;
    }
    Some(resolved)
}

pub(super) fn dash_fragment_duration(fragment: &Value) -> Option<Option<u64>> {
    let Some(value) = fragment.get("duration") else {
        return Some(None);
    };
    if value.is_null() {
        return Some(None);
    }
    let seconds = value.as_f64()?;
    if !seconds.is_finite() || seconds < 0.0 || seconds > u64::MAX as f64 / 1000.0 {
        return None;
    }
    Some(Some((seconds * 1000.0).round() as u64))
}

pub(super) fn dash_fragment_byte_range(fragment: &Value) -> Result<Option<String>> {
    let mut fields = std::collections::BTreeMap::new();
    for name in [
        "byte_range",
        "range",
        "http_range",
        "range_start",
        "range_end",
    ] {
        if let Some(value) = fragment.get(name) {
            fields.insert(name, value);
        }
    }
    if fields.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(&fields)?))
    }
}

pub(super) fn pin_selected_dash_format(
    inspection: &mut Value,
    entry_index: Option<usize>,
    format_id: &str,
    format: &Value,
    protocol: &str,
    control: &TransferControl,
    budget: &mut EvidenceBudget,
) -> Result<Option<StreamGraphEvidence>> {
    if format
        .get("extra_param_to_segment_url")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        // Applying extractor-specific query mutation ourselves would duplicate
        // downloader semantics. Such graphs remain an explicit-restart case.
        return Ok(None);
    }
    let fragment_base = match format.get("fragment_base_url") {
        Some(Value::String(value)) => {
            let Ok(url) = Url::parse(value) else {
                return Ok(None);
            };
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Ok(None);
            }
            Some(url)
        }
        Some(Value::Null) | None => None,
        Some(_) => return Ok(None),
    };
    let Some(fragments) = format.get("fragments").and_then(Value::as_array) else {
        return Ok(None);
    };
    if fragments.is_empty() {
        return Ok(None);
    }

    let mut normalized = Vec::with_capacity(fragments.len());
    let mut resources = Vec::with_capacity(fragments.len());
    for (sequence, fragment) in fragments.iter().enumerate() {
        if control.stop_requested() {
            bail!("Manifest kaynak kanıtı alınırken işlem durduruldu");
        }
        let Some(mut object) = fragment.as_object().cloned() else {
            return Ok(None);
        };
        let Some(resolved) = dash_fragment_url(fragment, fragment_base.as_ref()) else {
            return Ok(None);
        };
        let Some(duration_millis) = dash_fragment_duration(fragment) else {
            return Ok(None);
        };
        let byte_range = dash_fragment_byte_range(fragment)?;
        let resource = resolved.to_string();
        object.insert("url".into(), Value::String(resource.clone()));
        object.remove("path");
        let normalized_fragment = Value::Object(object);
        let context = serde_json::json!({
            "format_id": format_id,
            "protocol": protocol,
            "sequence": sequence,
            "fragment": &normalized_fragment,
        });
        resources.push(DashResourceEvidence {
            sequence,
            resource_sha256: sha256_text(&resource),
            initialization: fragment
                .get("initialization")
                .or_else(|| fragment.get("is_initialization"))
                .and_then(Value::as_bool),
            duration_millis,
            byte_range,
            context_sha256: sha256_bytes(&serde_json::to_vec(&context)?),
        });
        normalized.push(normalized_fragment);
    }
    let graph_bytes = serde_json::to_vec(&normalized)?
        .len()
        .checked_add(serde_json::to_vec(&resources)?.len());
    if !graph_bytes.is_some_and(|bytes| budget.account_bytes(bytes)) {
        return Ok(None);
    }

    let Some(object) =
        selected_format_mut(inspection, entry_index, format_id).and_then(Value::as_object_mut)
    else {
        return Ok(None);
    };
    let Some(first_url) = normalized[0]
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(None);
    };
    object.insert("url".into(), Value::String(first_url));
    object.insert("fragments".into(), Value::Array(normalized));
    // Native DASH consumes this authoritative finite array. Removing every MPD
    // locator prevents a load-info download from reparsing a changed manifest.
    object.remove("manifest_url");
    object.remove("fragment_base_url");
    object.remove("manifest_stream_number");
    Ok(Some(StreamGraphEvidence::Dash { resources }))
}

pub(super) fn manifest_snapshot(
    request: &AddRequest,
    format: &Value,
    target: &str,
    protocol: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
    budget: &mut EvidenceBudget,
) -> Result<Option<ManifestCapture>> {
    if !protocol.contains("m3u8") {
        return Ok(None);
    }
    let Some(timeout) = budget.request_timeout() else {
        return Ok(None);
    };
    let source = Url::parse(&request.url).context("Ana manifest adresi geçersiz")?;
    let target_url = Url::parse(target).context("Seçilen alt manifest adresi geçersiz")?;
    let same_origin = source.scheme() == target_url.scheme()
        && source.host_str() == target_url.host_str()
        && source.port_or_known_default() == target_url.port_or_known_default();
    if !matches!(target_url.scheme(), "http" | "https")
        || !target_url.username().is_empty()
        || target_url.password().is_some()
    {
        return Ok(None);
    }
    // Root request headers and referers cannot be assumed safe for another origin.
    // Cross-origin CDN manifests are capturable only when access is represented by
    // independently scoped cookies and non-sensitive format headers.
    if !same_origin && (!request.headers.is_empty() || request.referer.is_some()) {
        return Ok(None);
    }
    // Captured manifests reuse the handoff's cookies, so they are authorized by the page
    // those cookies were collected on rather than by the manifest request's own Referer.
    validate_scoped_cookies(&request.session_cookies, request.page_url.as_deref())?;
    let applicable_cookies = request
        .session_cookies
        .iter()
        .filter(|cookie| scoped_cookie_applies(cookie, &target_url))
        .collect::<Vec<_>>();
    let cookie_jar = if applicable_cookies.is_empty() {
        None
    } else {
        Some(TemporaryCookieJar(write_cookie_jar(applicable_cookies)?))
    };
    let mut easy = network.easy_for_url(&target_url, control)?;
    easy.url(target)?;
    easy.useragent("SSDownload/1.4")?;
    easy.follow_location(false)?;
    easy.fail_on_error(false)?;
    easy.connect_timeout(timeout.min(Duration::from_secs(5)))?;
    easy.timeout(timeout)?;
    easy.progress(true)?;
    if let Some(referer) = &request.referer {
        easy.referer(referer)?;
    }
    if let Some(cookie_jar) = &cookie_jar {
        easy.cookie_file(&cookie_jar.0)?;
    }
    let mut headers = List::new();
    if let Some(stream_headers) = format.get("http_headers").and_then(Value::as_object) {
        for (name, value) in stream_headers {
            let Some(value) = value.as_str() else {
                continue;
            };
            if name.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
                || value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
                || name.eq_ignore_ascii_case("cookie")
                || (!same_origin
                    && !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "accept" | "accept-language" | "user-agent" | "range"
                    ))
            {
                continue;
            }
            headers.append(&format!("{name}: {value}"))?;
        }
    }
    for (name, value) in &request.headers {
        headers.append(&format!("{name}: {value}"))?;
    }
    headers.append("X-SSDownload-Source-Evidence: 1")?;
    easy.http_headers(headers)?;
    let mut body = Vec::new();
    let mut oversized = false;
    let remaining_bytes = budget.remaining_bytes();
    if remaining_bytes == 0 {
        return Ok(None);
    }
    {
        let mut transfer = easy.transfer();
        transfer.progress_function(|_, _, _, _| {
            !control.stop_requested() && Instant::now() < budget.deadline
        })?;
        transfer.write_function(|data| {
            if body.len().saturating_add(data.len()) > remaining_bytes {
                oversized = true;
                return Ok(0);
            }
            body.extend_from_slice(data);
            Ok(data.len())
        })?;
        if transfer.perform().is_err() {
            if control.stop_requested() {
                bail!("Manifest kaynak kanıtı alınırken işlem durduruldu");
            }
            return Ok(None);
        }
    }
    if control.stop_requested() {
        bail!("Manifest kaynak kanıtı alınırken işlem durduruldu");
    }
    if oversized || !budget.account_bytes(body.len()) {
        return Ok(None);
    }
    let status = easy.response_code()?;
    if !(200..300).contains(&status) {
        return Ok(None);
    }
    let text = std::str::from_utf8(&body).context("Manifest UTF-8 değil")?;
    if !text.trim_start().starts_with("#EXTM3U")
        || text.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("#EXT-X-STREAM-INF") || line.starts_with("#EXT-X-MEDIA:")
        })
    {
        return Ok(None);
    }
    let pinned_body = absolutize_hls_manifest(text, &target_url)?;
    let mut resolution_base = target_url;
    resolution_base.set_query(None);
    resolution_base.set_fragment(None);
    Ok(Some(ManifestCapture {
        evidence: ManifestEvidence {
            resolution_base_sha256: sha256_text(resolution_base.as_str()),
            body_sha256: sha256_text(&pinned_body),
            bytes: pinned_body.len(),
        },
        body: pinned_body,
    }))
}

pub(super) fn absolutize_hls_manifest(text: &str, base: &Url) -> Result<String> {
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        let rewritten = if trimmed.is_empty() {
            String::new()
        } else if trimmed.starts_with('#') {
            rewrite_hls_uri_attributes(line, base)?
        } else {
            resolve_hls_uri(trimmed, base)?
        };
        output.push_str(&rewritten);
        output.push('\n');
    }
    Ok(output)
}

pub(super) fn rewrite_hls_uri_attributes(line: &str, base: &Url) -> Result<String> {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("URI=\"") {
        let value_start = start + 5;
        let Some(value_end) = rest[value_start..].find('"') else {
            bail!("HLS URI alanı kapatılmamış");
        };
        output.push_str(&rest[..value_start]);
        output.push_str(&resolve_hls_uri(
            &rest[value_start..value_start + value_end],
            base,
        )?);
        output.push('"');
        rest = &rest[value_start + value_end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

pub(super) fn resolve_hls_uri(value: &str, base: &Url) -> Result<String> {
    if value.starts_with("data:") {
        return Ok(value.to_string());
    }
    let resolved = base.join(value).context("HLS parça adresi çözümlenemedi")?;
    if !matches!(resolved.scheme(), "http" | "https")
        || !resolved.username().is_empty()
        || resolved.password().is_some()
    {
        bail!("HLS parça adresi güvenli HTTP/HTTPS kapsamının dışında");
    }
    Ok(resolved.into())
}

pub(super) fn sha256_text(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

pub(super) fn sha256_bytes(value: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value))
}

pub(super) fn selected_format_ids(selector: &str, info: &MediaInfo) -> Result<Vec<String>> {
    for alternative in selector.split('/') {
        let mut selected = Vec::new();
        let mut valid = true;
        for term in alternative.split('+') {
            let term = term.trim();
            if let Some(format) = info.formats.iter().find(|format| format.id == term) {
                selected.push(format.id.clone());
                continue;
            }
            let base = term.split('[').next().unwrap_or(term);
            let height_exact = selector_filter_u32(term, "height=");
            let height_max = selector_filter_u32(term, "height<=");
            let extension = selector_filter_text(term, "ext=");
            let language = selector_filter_text(term, "language=");
            let candidate = info.formats.iter().rev().find(|format| {
                let kind = match base {
                    "bestvideo" => {
                        stream_present(format.video_codec.as_deref())
                            && stream_absent(format.audio_codec.as_deref())
                    }
                    "bestaudio" => {
                        !stream_absent(format.audio_codec.as_deref())
                            && stream_absent(format.video_codec.as_deref())
                    }
                    "best" => {
                        !stream_absent(format.video_codec.as_deref())
                            && !stream_absent(format.audio_codec.as_deref())
                    }
                    _ => false,
                };
                kind && height_exact.is_none_or(|height| format.height == Some(height))
                    && height_max
                        .is_none_or(|height| format.height.is_none_or(|actual| actual <= height))
                    && extension
                        .as_ref()
                        .is_none_or(|value| format.extension == *value)
                    && language.as_ref().is_none_or(|value| {
                        format.language.as_deref().map(language_key) == Some(language_key(value))
                    })
            });
            if let Some(candidate) = candidate {
                selected.push(candidate.id.clone());
            } else {
                valid = false;
                break;
            }
        }
        if valid && !selected.is_empty() {
            return Ok(selected);
        }
    }
    bail!("Seçilen medya akışları kaynak kanıtı için kesin olarak çözümlenemedi")
}

pub(super) fn selector_filter_u32(selector: &str, marker: &str) -> Option<u32> {
    selector_filter_text(selector, marker)?.parse().ok()
}

pub(super) fn selector_filter_text(selector: &str, marker: &str) -> Option<String> {
    let start = selector.find(marker)? + marker.len();
    let value = &selector[start..];
    Some(value.split(']').next()?.to_string())
}

pub(super) fn read_media_source_state(workspace: &Path) -> Result<MediaSourceState> {
    let state: MediaSourceState = serde_json::from_slice(
        &fs::read(workspace.join(MEDIA_SOURCE_FILE))
            .context("Medya kaynak/parça kimliği kaydı okunamadı")?,
    )
    .context("Medya kaynak/parça kimliği kaydı bozuk")?;
    if state.version != MEDIA_SOURCE_STATE_VERSION {
        bail!("Medya kaynak/parça kimliği kayıt sürümü desteklenmiyor")
    }
    Ok(state)
}

pub(super) fn write_media_source_state(workspace: &Path, state: &MediaSourceState) -> Result<()> {
    crate::recovery::atomic_write(
        &workspace.join(MEDIA_SOURCE_FILE),
        &serde_json::to_vec(state)?,
    )
}

pub(super) fn value_contains_exact_marker(value: &Value, marker: &str) -> bool {
    match value {
        Value::String(text) => text == marker,
        Value::Array(values) => values
            .iter()
            .any(|value| value_contains_exact_marker(value, marker)),
        Value::Object(values) => values
            .values()
            .any(|value| value_contains_exact_marker(value, marker)),
        _ => false,
    }
}

pub(super) fn validate_pinned_source(source: &PinnedMediaSource) -> Result<()> {
    if source.manifests.is_empty() && source.dash_formats.is_empty() {
        bail!("Doğrulanmış medya kaynak paketi seçili akış içermiyor");
    }
    let mut manifest_markers = HashSet::new();
    for manifest in &source.manifests {
        if !manifest.marker.starts_with(PINNED_URL_PREFIX)
            || !manifest_markers.insert(manifest.marker.as_str())
            || !value_contains_exact_marker(&source.inspection, &manifest.marker)
        {
            bail!("Doğrulanmış HLS manifest paketi seçili akışla eşleşmiyor");
        }
    }
    let mut dash_identities = HashSet::new();
    for pinned in &source.dash_formats {
        if !dash_identities.insert((pinned.entry_index, pinned.format_id.as_str())) {
            bail!("Doğrulanmış DASH paketi yinelenen akış içeriyor");
        }
        let Some(format) =
            selected_format(&source.inspection, pinned.entry_index, &pinned.format_id)
        else {
            bail!("Doğrulanmış DASH paketi seçili akışı içermiyor");
        };
        let protocol = format
            .get("protocol")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(fragments) = format.get("fragments").and_then(Value::as_array) else {
            bail!("Doğrulanmış DASH paketi sonlu parça listesi içermiyor");
        };
        if !protocol.contains("dash")
            || protocol.contains("generator")
            || fragments.is_empty()
            || format.get("manifest_url").is_some()
            || format.get("fragment_base_url").is_some()
            || fragments.iter().any(|fragment| {
                fragment.get("path").is_some() || dash_fragment_url(fragment, None).is_none()
            })
        {
            bail!("Doğrulanmış DASH paketi sabitlenmiş sonlu parça grafiği içermiyor");
        }
        if format.get("url").and_then(Value::as_str)
            != fragments[0].get("url").and_then(Value::as_str)
        {
            bail!("Doğrulanmış DASH paketinin akış adresi parça grafiğiyle eşleşmiyor");
        }
    }
    Ok(())
}

pub(super) fn load_pinned_source_file(path: &Path) -> Result<PinnedMediaSource> {
    let sealed = fs::read_to_string(path).context("Doğrulanmış medya kaynak paketi okunamadı")?;
    let opened =
        crate::secure::unseal(&sealed).context("Doğrulanmış medya kaynak paketi açılamadı")?;
    let source: PinnedMediaSource =
        serde_json::from_str(&opened).context("Doğrulanmış medya kaynak paketi bozuk")?;
    validate_pinned_source(&source)?;
    Ok(source)
}

pub(super) fn load_pinned_source_for_request(
    output: &Path,
    request: &AddRequest,
) -> Result<Option<(PinnedMediaSource, MediaSourceEvidence)>> {
    let owner = crate::output::identity(output)?;
    let workspace = workspace_path(output, &owner);
    if !workspace.exists() || !workspace.join(MEDIA_SOURCE_FILE).exists() {
        return Ok(None);
    }
    verify_workspace(&workspace, &owner)?;
    let fingerprint = request_fingerprint(request)?;
    let current = read_media_source_state(&workspace)?;
    if current.request == fingerprint {
        let pinned = load_pinned_source_file(&workspace.join(MEDIA_PINNED_FILE))?;
        return Ok(Some((pinned, current.evidence)));
    }
    let pending_path = workspace.join(MEDIA_SOURCE_PENDING_FILE);
    if !pending_path.exists() {
        return Ok(None);
    }
    let pending: PendingMediaSourceState = serde_json::from_slice(&fs::read(&pending_path)?)
        .context("Yenilenen HLS kaynak geçiş kaydı bozuk")?;
    if pending.version == MEDIA_SOURCE_STATE_VERSION
        && pending.previous_request == current.request
        && pending.next.request == fingerprint
        && !pending.next.evidence.live
    {
        let pinned = load_pinned_source_file(&workspace.join(MEDIA_PINNED_PENDING_FILE))?;
        return Ok(Some((pinned, pending.next.evidence)));
    }
    Ok(None)
}
