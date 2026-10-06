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
            anyhow!("Seçilen kalite/biçim kullanılabilir DRM'siz formatlar arasında değil: {id}")
        })?;
        if format.has_drm {
            crate::logging::record(
                crate::logging::Event::failure(
                    "inspect.drm",
                    crate::error_codes::MED_003,
                    format!("seçilen biçim DRM korumalı: {id}"),
                )
                .host(crate::logging::host_of(&request.url).unwrap_or_default()),
            );
            crate::bail_code!(crate::error_codes::MED_003);
        }
        return match request.kind {
            DownloadKind::Video if stream_absent(format.video_codec.as_deref()) => {
                bail!("Seçilen format video akışı içermiyor")
            }
            DownloadKind::Audio => {
                if stream_absent(format.audio_codec.as_deref()) {
                    bail!("Seçilen format ses akışı içermiyor");
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
    // Direct media URLs have no extractor format table or resolution fields.
    // Applying bestvideo[height<=…] to them makes yt-dlp fail with “Requested
    // format is not available”, even though the source is downloadable. Let
    // FFmpeg remux the single source into the requested container instead.
    if direct_media_url(&request.url) {
        return Ok(if request.container.as_deref() == Some("mp4") {
            "best[ext=mp4]/best".into()
        } else {
            "best".into()
        });
    }
    let height = request
        .max_height
        .map(|h| format!("[height<={h}]"))
        .unwrap_or_default();
    let video = format!("bestvideo{height}");
    let combined = format!("best{height}");
    if request.expected_audio != Some(true) && (explicitly_silent(info) || !audio_offered(info)) {
        return Ok(format!("{video}/{combined}"));
    }
    Ok(if request.container.as_deref() == Some("mp4") {
        automatic_mp4_selector(request, info)?
    } else {
        format!("{video}+bestaudio/{combined}")
    })
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

// Missing extractor metadata is not evidence of codec incompatibility. Unknown
// formats remain provisional: validate_probe checks the actual output codecs.
pub(super) fn mp4_video_stream(format: &MediaFormat) -> bool {
    format.video_codec.as_deref().is_none_or(mp4_video_codec)
}

pub(super) fn mp4_audio_stream(format: &MediaFormat) -> bool {
    format.audio_codec.as_deref().is_none_or(mp4_audio_codec)
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
        .max_by(|left, right| {
            (left.height.unwrap_or(0), left.width.unwrap_or(0))
                .cmp(&(right.height.unwrap_or(0), right.width.unwrap_or(0)))
        })
        .ok_or_else(|| {
            anyhow!("Seçilen çözünürlükte MP4 uyumlu görüntü codec'i bulunamadı; MKV seçin")
        })
}

pub(super) fn best_mp4_audio<'a>(
    request: &AddRequest,
    info: &'a MediaInfo,
) -> Result<&'a MediaFormat> {
    info.formats
        .iter()
        .filter(|format| mp4_audio_candidate(format, request))
        .max_by_key(|format| {
            (
                format.audio_channels.unwrap_or(0),
                format.filesize.unwrap_or(0),
            )
        })
        .ok_or_else(|| anyhow!("Seçilen dilde MP4 uyumlu ses codec'i bulunamadı; MKV seçin"))
}

pub(super) fn automatic_mp4_selector(request: &AddRequest, info: &MediaInfo) -> Result<String> {
    let video_only = info
        .formats
        .iter()
        .filter(|format| mp4_video_candidate(format, request))
        .max_by(|left, right| {
            (left.height.unwrap_or(0), left.width.unwrap_or(0))
                .cmp(&(right.height.unwrap_or(0), right.width.unwrap_or(0)))
        });
    let audio_only = info
        .formats
        .iter()
        .filter(|format| mp4_audio_candidate(format, request))
        .max_by_key(|format| {
            (
                format.audio_channels.unwrap_or(0),
                format.filesize.unwrap_or(0),
            )
        });
    if let (Some(video), Some(audio)) = (video_only, audio_only) {
        return Ok(format!("{}+{}", video.id, audio.id));
    }
    let combined = info
        .formats
        .iter()
        .filter(|format| {
            mp4_video_stream(format)
                && mp4_audio_stream(format)
                && requested_video_size(format, request)
        })
        .max_by(|left, right| {
            (left.height.unwrap_or(0), left.width.unwrap_or(0))
                .cmp(&(right.height.unwrap_or(0), right.width.unwrap_or(0)))
        });
    if let Some(combined) = combined {
        return Ok(combined.id.clone());
    }
    if request.expected_audio != Some(true) && (explicitly_silent(info) || !audio_offered(info)) {
        if let Some(video) = video_only {
            return Ok(video.id.clone());
        }
    }
    crate::bail_code!(
        crate::error_codes::MED_014,
        "Kaynakta MP4 kapsayıcısına kayıpsız aktarılabilen bir akış birleşimi yok; MKV seçin"
    )
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

    let format_compatible = |format: &MediaFormat| {
        let video_ok = stream_absent(format.video_codec.as_deref()) || mp4_video_stream(format);
        let audio_ok = stream_absent(format.audio_codec.as_deref()) || mp4_audio_stream(format);
        video_ok && audio_ok
    };
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
                .ok_or_else(|| anyhow!("Seçilen biçim MP4 uyumluluğu için bulunamadı"))?,
        );
    }
    if let Some(id) = &request.audio_format_id {
        selected.push(exact_audio_format(info, id)?);
    }
    for audio in &request.audio_tracks {
        selected.push(exact_audio_format(info, &audio.id)?);
    }
    if selected.iter().any(|format| !format_compatible(format)) {
        crate::bail_code!(crate::error_codes::MED_014, "Seçilen codec birleşimi MP4 kapsayıcısına kayıpsız aktarılamıyor; MKV seçin. Sessiz yeniden kodlama yapılmadı");
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
        crate::bail_code!(crate::error_codes::MED_014, "Kaynakta MP4 kapsayıcısına kayıpsız aktarılabilen görüntü/ses birleşimi yok; MKV seçin. Sessiz yeniden kodlama yapılmadı");
    }
    Ok(())
}

pub(super) fn mp4_video_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    [
        "avc", "h264", "hevc", "h265", "hev1", "hvc1", "av01", "av1", "mpeg4", "mp4v",
    ]
    .iter()
    .any(|prefix| codec.starts_with(prefix))
}

pub(super) fn mp4_audio_codec(codec: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    ["aac", "mp4a", "alac", "mp3", "ac3", "eac3"]
        .iter()
        .any(|prefix| codec.starts_with(prefix))
}

pub(super) fn is_hdr_dynamic_range(value: &str) -> bool {
    let upper = value.to_ascii_uppercase();
    ["HDR", "HLG", "DOLBY VISION", "PQ"]
        .iter()
        .any(|marker| upper.contains(marker))
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
