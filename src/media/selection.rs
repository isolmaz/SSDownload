//! Format selector construction and container/codec compatibility of a selection.

use super::*;

pub(super) fn format_selector(request: &AddRequest, info: &MediaInfo) -> Result<String> {
    if request.kind == DownloadKind::Video
        && request.container.as_deref() == Some("mp4")
        && request.exact_height.is_some()
        && request.video_format_id.is_none()
        && request.audio_format_id.is_none()
        && request.audio_language.is_none()
        && request.audio_tracks.is_empty()
    {
        validate_track_selection(request, info)?;
        return automatic_mp4_selector(request, info);
    }
    if request.video_format_id.is_some()
        || request.audio_format_id.is_some()
        || request.audio_language.is_some()
        || !request.audio_tracks.is_empty()
        || request.exact_height.is_some()
    {
        return selected_tracks_format(request, info);
    }
    if let Some(id) = request
        .format_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let format = info.formats.iter().find(|f| f.id == id).ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "Seçilen kalite/biçim kullanılabilir DRM'siz formatlar arasında değil: {id}",
                    "The selected quality/format is not among the available DRM-free formats: {id}",
                    id = id
                )
            )
        })?;
        if format.has_drm {
            crate::logging::record(
                crate::logging::Event::failure(
                    "inspect.drm",
                    crate::error_codes::MED_003,
                    format!("the selected format is DRM protected: {id}"),
                )
                .host(crate::logging::host_of(&request.url).unwrap_or_default()),
            );
            crate::bail_code!(crate::error_codes::MED_003);
        }
        return match request.kind {
            DownloadKind::Video if stream_absent(format.video_codec.as_deref()) => {
                bail!(crate::i18n::ui(
                    "Seçilen format video akışı içermiyor",
                    "The selected format has no video stream"
                ))
            }
            DownloadKind::Audio => {
                if stream_absent(format.audio_codec.as_deref()) {
                    bail!(crate::i18n::ui(
                        "Seçilen format ses akışı içermiyor",
                        "The selected format has no audio stream"
                    ));
                }
                Ok(id.to_string())
            }
            _ if stream_absent(format.audio_codec.as_deref())
                && !stream_absent(format.video_codec.as_deref()) =>
            {
                // Pair with bestaudio only when the manifest actually offers audio.
                // Child-variant HLS manifests (video-only, unknown codecs) would otherwise
                // fail yt-dlp with "Requested format is not available" before any byte.
                if !audio_offered(info) {
                    Ok(id.to_string())
                } else {
                    Ok(format!("{id}+bestaudio"))
                }
            }
            _ => Ok(id.to_string()),
        };
    }
    if !info.formats.is_empty() {
        if request.kind == DownloadKind::Video
            && info
                .formats
                .iter()
                .all(|f| stream_absent(f.video_codec.as_deref()))
        {
            crate::bail_code!(crate::error_codes::MED_001);
        }
        if request.kind == DownloadKind::Audio
            && info
                .formats
                .iter()
                .all(|f| stream_absent(f.audio_codec.as_deref()))
        {
            crate::bail_code!(crate::error_codes::MED_001);
        }
    }
    if request.kind == DownloadKind::Audio {
        // The generic extractor represents a direct MP4/MKV URL as one
        // `best` format with unknown codecs. `bestaudio` then rejects a
        // perfectly valid source before FFmpeg can extract its audio.
        if direct_media_url(&request.url) {
            return Ok("best".into());
        }
        return Ok("bestaudio/best".into());
    }
    // A bare media file has no extractor format table or resolution fields.
    // Applying bestvideo[height<=…] to it makes yt-dlp fail with “Requested
    // format is not available”, even though the source is downloadable. Let
    // FFmpeg remux the single source into the requested container instead.
    if bare_media_file(request, info) {
        return Ok(if request.container.as_deref() == Some("mp4") {
            "best[ext=mp4]/best".into()
        } else {
            "best".into()
        });
    }
    if request.container.as_deref() == Some("mp4") {
        return automatic_mp4_selector(request, info);
    }
    let silent = video_only_source(request, info);
    let expression = |height: &str| {
        if silent {
            format!("bestvideo{height}/best{height}")
        } else {
            format!("bestvideo{height}+bestaudio/best{height}")
        }
    };
    Ok(match request.max_height {
        // A format that reports no height is the last resort under a limit.
        Some(maximum) => format!(
            "{}/{}",
            expression(&format!("[height<={maximum}]")),
            expression(&format!("[height<=?{maximum}]"))
        ),
        None => expression(""),
    })
}

/// The formats a download of `request` takes from `info`, resolved as the download selects
/// them; empty when that selection fails.
pub(crate) fn selected_formats<'a>(
    request: &AddRequest,
    info: &'a MediaInfo,
) -> Vec<&'a MediaFormat> {
    let Ok(ids) =
        format_selector(request, info).and_then(|selector| selected_format_ids(&selector, info))
    else {
        return Vec::new();
    };
    ids.iter()
        .filter_map(|id| info.formats.iter().find(|format| format.id == *id))
        .collect()
}

pub(super) fn requested_video_size(format: &MediaFormat, request: &AddRequest) -> bool {
    request
        .exact_height
        .is_none_or(|height| format.height == Some(height))
        && request
            .max_height
            .is_none_or(|maximum| format.height.is_none_or(|height| height <= maximum))
}

pub(super) fn mp4_video_candidate(format: &MediaFormat, request: &AddRequest) -> bool {
    mp4_video_stream(format)
        && stream_absent(format.audio_codec.as_deref())
        && requested_video_size(format, request)
}

/// How far the analysis shows that a stream goes into MP4 without re-encoding. A reported codec
/// decides. Without one the format's extension is the only evidence: a container of MP4's own
/// family counts as a fit, one that carries codecs MP4 cannot hold rules the stream out, and any
/// other extension leaves it open for `validate_probe` to check on the output.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mp4Fit {
    No,
    Unproven,
    Yes,
}

/// Extensions of containers that share MP4's codecs.
const MP4_FAMILY_EXTENSIONS: &[&str] = &["mp4", "m4v", "m4a", "mov"];

/// Extensions of containers whose streams (Theora, Vorbis, Opus, VP8, VP9, Windows Media, FLAC)
/// MP4 cannot take without re-encoding.
const NON_MP4_EXTENSIONS: &[&str] = &[
    "ogv", "ogg", "oga", "ogx", "opus", "webm", "weba", "wmv", "wma", "asf", "flac",
];

fn mp4_fit(codec: Option<&str>, extension: &str, codec_fits: fn(&str) -> bool) -> Mp4Fit {
    match codec {
        Some(codec) if codec_fits(codec) => Mp4Fit::Yes,
        Some(_) => Mp4Fit::No,
        None if MP4_FAMILY_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension)) =>
        {
            Mp4Fit::Yes
        }
        None if NON_MP4_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension)) =>
        {
            Mp4Fit::No
        }
        None => Mp4Fit::Unproven,
    }
}

fn mp4_video_fit(format: &MediaFormat) -> Mp4Fit {
    mp4_fit(
        format.video_codec.as_deref(),
        &format.extension,
        mp4_video_codec,
    )
}

fn mp4_audio_fit(format: &MediaFormat) -> Mp4Fit {
    mp4_fit(
        format.audio_codec.as_deref(),
        &format.extension,
        mp4_audio_codec,
    )
}

pub(super) fn mp4_video_stream(format: &MediaFormat) -> bool {
    mp4_video_fit(format) != Mp4Fit::No
}

pub(super) fn mp4_audio_stream(format: &MediaFormat) -> bool {
    mp4_audio_fit(format) != Mp4Fit::No
}

/// Whether every stream `format` carries goes into `container` without re-encoding. Only MP4
/// limits the codecs.
pub(crate) fn format_fits_container(format: &MediaFormat, container: Option<&str>) -> bool {
    container != Some("mp4")
        || ((stream_absent(format.video_codec.as_deref()) || mp4_video_stream(format))
            && (stream_absent(format.audio_codec.as_deref()) || mp4_audio_stream(format)))
}

pub(super) fn mp4_audio_candidate(format: &MediaFormat, request: &AddRequest) -> bool {
    stream_absent(format.video_codec.as_deref())
        && mp4_audio_stream(format)
        && request.audio_language.as_ref().is_none_or(|language| {
            format.language.as_deref().map(language_key) == Some(language_key(language))
        })
}

pub(super) fn best_mp4_video<'a>(
    request: &AddRequest,
    info: &'a MediaInfo,
) -> Result<&'a MediaFormat> {
    info.formats
        .iter()
        .filter(|format| mp4_video_candidate(format, request))
        .max_by_key(|format| (mp4_video_fit(format), pixel_size(format)))
        .ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "Seçilen çözünürlükte MP4 uyumlu görüntü codec'i bulunamadı; MKV seçin",
                "No MP4-compatible video codec was found at the selected resolution; choose MKV"
            ))
        })
}

/// The pixels of a format, tallest first when compared; a format that reports none is the
/// smallest.
fn pixel_size(format: &MediaFormat) -> (u32, u32) {
    (format.height.unwrap_or(0), format.width.unwrap_or(0))
}

/// The extractor lists formats from worst to best in its own preference order (the original
/// language before dubs, DRC variants last), so among the audio formats that fit equally well the
/// last one is its pick.
fn preferred_mp4_audio<'a>(request: &AddRequest, info: &'a MediaInfo) -> Option<&'a MediaFormat> {
    info.formats
        .iter()
        .enumerate()
        .filter(|(_, format)| mp4_audio_candidate(format, request))
        .max_by_key(|(position, format)| (mp4_audio_fit(format), *position))
        .map(|(_, format)| format)
}

pub(super) fn best_mp4_audio<'a>(
    request: &AddRequest,
    info: &'a MediaInfo,
) -> Result<&'a MediaFormat> {
    preferred_mp4_audio(request, info).ok_or_else(|| {
        anyhow!(crate::i18n::ui(
            "Seçilen dilde MP4 uyumlu ses codec'i bulunamadı; MKV seçin",
            "No MP4-compatible audio codec was found in the selected language; choose MKV"
        ))
    })
}

/// A playlist is downloaded with one `--format` expression for every entry, and format ids
/// belong to a single entry; the codecs, the height and the audio language take the place of
/// the ids. The formats whose reported codec fits MP4 come first, whatever their extension, as
/// [`mp4_fit`] ranks them for a single item. The fallback lets a format pass whose codec the
/// extractor did not report (an HLS audio rendition has no `acodec`; `?` after `~=` and `<=`)
/// unless its extension is in [`NON_MP4_EXTENSIONS`], which is how [`mp4_fit`] judges a format
/// that names no codec. An exact height must be reported. Under a height limit the formats that
/// report a height come first and the ones that do not follow, as in [`format_selector`].
pub(super) fn playlist_mp4_selector(request: &AddRequest, with_audio: bool) -> String {
    let exact = request
        .exact_height
        .map(|exact| format!("[height={exact}]"))
        .unwrap_or_default();
    let language = request
        .audio_language
        .as_ref()
        .map(|language| format!("[language={language}]"))
        .unwrap_or_default();
    let extension: String = NON_MP4_EXTENSIONS
        .iter()
        .map(|excluded| format!("[ext!={excluded}]"))
        .collect();
    let expression = |height: &str, unreported: &str, extension: &str| {
        let video = format!("[vcodec~={unreported}'^({})']", MP4_VIDEO_CODECS.join("|"));
        let audio = format!(
            "[acodec~={unreported}'^({})']{language}",
            MP4_AUDIO_CODECS.join("|")
        );
        if with_audio {
            format!(
                "bestvideo{height}{video}{extension}+bestaudio{audio}{extension}\
                 /best{height}{video}{audio}{extension}"
            )
        } else {
            format!("bestvideo{height}{video}{extension}")
        }
    };
    let stage = |height: &str| {
        format!(
            "{}/{}",
            expression(height, "", ""),
            expression(height, "?", &extension)
        )
    };
    match request.max_height {
        Some(maximum) => format!(
            "{}/{}",
            stage(&format!("{exact}[height<={maximum}]")),
            stage(&format!("{exact}[height<=?{maximum}]"))
        ),
        None => stage(&exact),
    }
}

/// The MP4-compatible selection resolved from the sampled entry: its format ids, or for a
/// playlist a codec-filtered expression that fits the other entries too. Under a height limit a
/// format that reports no height is the last resort, as in [`format_selector`].
pub(super) fn automatic_mp4_selector(request: &AddRequest, info: &MediaInfo) -> Result<String> {
    if request.max_height.is_some() {
        if let Some(selector) = mp4_selection(request, info, true) {
            return Ok(selector);
        }
    }
    if let Some(selector) = mp4_selection(request, info, false) {
        return Ok(selector);
    }
    crate::bail_code!(
        crate::error_codes::MED_014,
        "{}",
        crate::i18n::ui(
            "Kaynakta MP4 kapsayıcısına kayıpsız aktarılabilen bir akış birleşimi yok; MKV seçin",
            "The source has no stream combination that can be remuxed into MP4 without loss; choose MKV"
        )
    )
}

/// The selection of [`automatic_mp4_selector`] from the formats that fit the requested size; with
/// `reported_height` only from the ones that report a height.
fn mp4_selection(request: &AddRequest, info: &MediaInfo, reported_height: bool) -> Option<String> {
    let fits = |format: &MediaFormat| {
        requested_video_size(format, request) && (!reported_height || format.height.is_some())
    };
    let video_only = info
        .formats
        .iter()
        .filter(|format| mp4_video_candidate(format, request) && fits(format))
        .max_by_key(|format| (mp4_video_fit(format), pixel_size(format)));
    let audio_only = preferred_mp4_audio(request, info);
    if let (Some(video), Some(audio)) = (video_only, audio_only) {
        return Some(if request.playlist {
            playlist_mp4_selector(request, true)
        } else {
            format!("{}+{}", video.id, audio.id)
        });
    }
    let combined = info
        .formats
        .iter()
        .filter(|format| mp4_video_stream(format) && mp4_audio_stream(format) && fits(format))
        .max_by_key(|format| {
            (
                mp4_video_fit(format).min(mp4_audio_fit(format)),
                pixel_size(format),
            )
        });
    if let Some(combined) = combined {
        return Some(if request.playlist {
            playlist_mp4_selector(request, true)
        } else {
            combined.id.clone()
        });
    }
    if video_only_source(request, info) {
        if let Some(video) = video_only {
            return Some(if request.playlist {
                playlist_mp4_selector(request, false)
            } else {
                video.id.clone()
            });
        }
    }
    None
}

pub(super) fn validate_container_selection(request: &AddRequest, info: &MediaInfo) -> Result<()> {
    let Some(container) = request.container.as_deref() else {
        return Ok(());
    };
    if request.kind == DownloadKind::Audio || container == "mkv" {
        return Ok(());
    }
    if container != "mp4" {
        return Ok(());
    }
    if direct_media_url(&request.url)
        && Url::parse(&request.url)
            .ok()
            .and_then(|url| url.path().rsplit('.').next().map(str::to_ascii_lowercase))
            .as_deref()
            == Some("mp4")
        && request.format_id.is_none()
        && request.video_format_id.is_none()
        && request.audio_tracks.is_empty()
        && request.audio_format_id.is_none()
    {
        return Ok(());
    }

    let mut selected = Vec::new();
    if let Some(id) = request
        .format_id
        .as_ref()
        .or(request.video_format_id.as_ref())
    {
        selected.push(
            info.formats
                .iter()
                .find(|format| format.id == *id)
                .ok_or_else(|| {
                    anyhow!(crate::i18n::ui(
                        "Seçilen biçim MP4 uyumluluğu için bulunamadı",
                        "The selected format was not found for MP4 compatibility"
                    ))
                })?,
        );
    }
    if let Some(id) = &request.audio_format_id {
        selected.push(exact_audio_format(info, id)?);
    }
    for audio in &request.audio_tracks {
        selected.push(exact_audio_format(info, &audio.id)?);
    }
    if selected
        .iter()
        .any(|format| !format_fits_container(format, Some("mp4")))
    {
        crate::bail_code!(
            crate::error_codes::MED_014,
            "{}",
            crate::i18n::ui(
                "Seçilen codec birleşimi MP4 kapsayıcısına kayıpsız aktarılamıyor; MKV seçin. Sessiz yeniden kodlama yapılmadı",
                "The selected codec combination cannot be remuxed into MP4 without loss; choose MKV. No silent re-encoding was done"
            )
        );
    }
    if !selected.is_empty() {
        return Ok(());
    }

    let video_available = info
        .formats
        .iter()
        .any(|format| mp4_video_stream(format) && requested_video_size(format, request));
    let audio_required = request.expected_audio == Some(true) || !explicitly_silent(info);
    let audio_available = !audio_required
        || info.formats.iter().any(|format| {
            mp4_audio_stream(format)
                && request.audio_language.as_ref().is_none_or(|language| {
                    format.language.as_deref().map(language_key) == Some(language_key(language))
                })
        });
    if !video_available || !audio_available {
        crate::bail_code!(
            crate::error_codes::MED_014,
            "{}",
            crate::i18n::ui(
                "Kaynakta MP4 kapsayıcısına kayıpsız aktarılabilen görüntü/ses birleşimi yok; MKV seçin. Sessiz yeniden kodlama yapılmadı",
                "The source has no video/audio combination that can be remuxed into MP4 without loss; choose MKV. No silent re-encoding was done"
            )
        );
    }
    Ok(())
}

const MP4_VIDEO_CODECS: &[&str] = &[
    "avc", "h264", "hevc", "h265", "hev1", "hvc1", "dvh1", "dvhe", "av01", "av1", "mpeg4", "mp4v",
];

/// Both FFmpeg names (`ac3`, `eac3`) and the RFC 6381 names yt-dlp reports (`ac-3`, `ec-3`).
const MP4_AUDIO_CODECS: &[&str] = &["aac", "mp4a", "alac", "mp3", "ac3", "ac-3", "eac3", "ec-3"];

pub(super) fn mp4_video_codec(codec: &str) -> bool {
    starts_with_any(codec, MP4_VIDEO_CODECS)
}

pub(super) fn mp4_audio_codec(codec: &str) -> bool {
    starts_with_any(codec, MP4_AUDIO_CODECS)
}

pub(super) fn starts_with_any<S: AsRef<str>>(codec: &str, prefixes: &[S]) -> bool {
    let codec = codec.to_ascii_lowercase();
    prefixes
        .iter()
        .any(|prefix| codec.starts_with(prefix.as_ref()))
}

pub(super) fn is_hdr_dynamic_range(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    ["HDR", "HLG", "DOLBY VISION", "PQ"]
        .iter()
        .any(|marker| upper.contains(marker))
}

/// Whether the source is a bare media file: its address names one and the analysis lists nothing
/// to choose from. An address that only ends in a media extension (the file page of a wiki, a
/// player route) is analysed into a format table, which the height limit and the codec rules
/// apply to.
fn bare_media_file(request: &AddRequest, info: &MediaInfo) -> bool {
    direct_media_url(&request.url) && info.formats.len() <= 1
}

/// Whether the video is all there is to download: no audio is expected, and the source offers
/// none, with every format silent or none of them audio-only or ready-made with audio. A
/// video-only format of a source that has ready-made formats would otherwise win over them and
/// lose the audio.
fn video_only_source(request: &AddRequest, info: &MediaInfo) -> bool {
    let carries_audio = info
        .formats
        .iter()
        .any(|format| stream_present(format.audio_codec.as_deref()));
    request.expected_audio != Some(true)
        && (explicitly_silent(info) || !(audio_offered(info) || carries_audio))
}

pub(super) fn explicitly_silent(info: &MediaInfo) -> bool {
    !info.formats.is_empty()
        && info
            .formats
            .iter()
            .all(|f| f.audio_codec.as_deref() == Some("none"))
}

// True when the manifest lists a separate audio-only variant (vcodec "none", e.g.
// an EXT-X-MEDIA audio group). yt-dlp reports such formats with unknown codecs, so
// the acodec field cannot be trusted; the video codec is the reliable marker.
// Pairing with bestaudio is only meaningful when such a variant exists, and a
// video-only output is only suspicious when it does.
pub(super) fn audio_offered(info: &MediaInfo) -> bool {
    info.formats
        .iter()
        .any(|f| stream_absent(f.video_codec.as_deref()))
}
