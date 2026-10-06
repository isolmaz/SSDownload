//! Extractor JSON to MediaInfo: formats, tolerances, DRM and live markers.

use super::*;

pub(super) fn media_info_from_json(root: &Value) -> Result<MediaInfo> {
    if explicit_drm(root) {
        crate::bail_code!(crate::error_codes::MED_003);
    }
    let is_playlist = root.get("_type").and_then(Value::as_str) == Some("playlist")
        || root.get("entries").and_then(Value::as_array).is_some();
    let entries: Vec<&Value> = root
        .get("entries")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|v| !v.is_null()).collect())
        .unwrap_or_default();
    if is_playlist && entries.is_empty() {
        // A page that is a feed rather than one media item (a site home, a search result
        // page, an empty playlist) resolves to an empty playlist. Say what to do instead
        // of reporting a bare "playlist" failure for a page the user opened normally.
        crate::logging::record(crate::logging::Event::failure(
            "inspect.feed_address",
            crate::error_codes::MED_002,
            "adres tek bir medya içermiyor",
        ));
        crate::bail_code!(crate::error_codes::MED_002);
    }
    for entry in &entries {
        if is_drm_only(entry) {
            let title = entry
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("adsız öğe");
            crate::logging::record(crate::logging::Event::failure(
                "inspect.drm",
                crate::error_codes::MED_004,
                format!("liste öğesi DRM korumalı: {title}"),
            ));
            crate::bail_code!(
                crate::error_codes::MED_004,
                "Liste DRM korumalı bir öğe içeriyor ve güvenle indirilemez: {title}"
            );
        }
    }
    if !is_playlist && is_drm_only(root) {
        crate::bail_code!(crate::error_codes::MED_003);
    }

    let format_source = if root.get("formats").and_then(Value::as_array).is_some() {
        root
    } else {
        entries
            .iter()
            .copied()
            .find(|entry| entry.get("formats").and_then(Value::as_array).is_some())
            .unwrap_or(root)
    };
    let formats = parse_formats(format_source);
    if !is_playlist && formats.is_empty() {
        crate::bail_code!(crate::error_codes::MED_001);
    }

    let subtitle_tracks: Vec<_> = [root]
        .into_iter()
        .chain(entries.iter().copied())
        .flat_map(|entry| {
            ["subtitles", "automatic_captions"]
                .into_iter()
                .flat_map(move |field| {
                    entry
                        .get(field)
                        .and_then(Value::as_object)
                        .into_iter()
                        .flat_map(move |values| {
                            values
                                .iter()
                                .filter(|(_, variants)| {
                                    // yt-dlp also reports JSON chat/event feeds here, but
                                    // its subtitle embedder cannot turn them into captions.
                                    variants.as_array().is_some_and(|variants| {
                                        variants.iter().any(|variant| {
                                            variant.get("ext").and_then(Value::as_str).is_some_and(
                                                |ext| !ext.eq_ignore_ascii_case("json"),
                                            )
                                        })
                                    })
                                })
                                .map(move |(language, _)| SubtitleTrack {
                                    language: language.clone(),
                                    automatic: field == "automatic_captions",
                                })
                        })
                })
        })
        .collect();
    let subtitles: BTreeSet<_> = subtitle_tracks
        .iter()
        .map(|track| track.language.clone())
        .collect();
    let duration = root.get("duration").and_then(Value::as_f64).or_else(|| {
        if entries.is_empty() {
            return None;
        }
        let durations: Vec<_> = entries
            .iter()
            .filter_map(|v| v.get("duration").and_then(Value::as_f64))
            .collect();
        (durations.len() == entries.len()).then(|| durations.iter().sum())
    });
    let is_live = live_value(root) || entries.iter().any(|entry| live_value(entry));
    Ok(MediaInfo {
        title: string_field(root, "title")
            .or_else(|| string_field(root, "playlist_title"))
            .unwrap_or_else(|| "Adsız medya".into()),
        webpage_url: string_field(root, "webpage_url")
            .or_else(|| string_field(root, "original_url"))
            .unwrap_or_default(),
        duration,
        duration_tolerance: fragment_duration_tolerance(root),
        thumbnail: string_field(root, "thumbnail")
            .or_else(|| entries.first().and_then(|e| string_field(e, "thumbnail"))),
        formats,
        subtitles: subtitles.into_iter().collect(),
        subtitle_tracks,
        playlist_count: if is_playlist {
            root.get("playlist_count")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .or(Some(entries.len()))
        } else {
            None
        },
        is_live,
        extractor: string_field(root, "extractor_key")
            .or_else(|| string_field(root, "extractor"))
            .unwrap_or_default(),
    })
}

pub(super) fn parse_formats(value: &Value) -> Vec<MediaFormat> {
    value
        .get("formats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|format| {
            let id = format.get("format_id").and_then(Value::as_str)?.to_string();
            let has_drm = explicit_drm(format);
            if has_drm {
                return None;
            }
            let video_codec = optional_codec(format, "vcodec");
            let audio_codec = optional_codec(format, "acodec");
            if stream_absent(video_codec.as_deref()) && stream_absent(audio_codec.as_deref()) {
                return None;
            }
            let width = as_u32(format.get("width"));
            let height = as_u32(format.get("height"));
            let fps = format.get("fps").and_then(Value::as_f64);
            let extension = format
                .get("ext")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let note = format
                .get("format_note")
                .and_then(Value::as_str)
                .unwrap_or("");
            let resolution = format
                .get("resolution")
                .and_then(Value::as_str)
                .filter(|value| *value != "unknown" && *value != note)
                .unwrap_or("");
            let video = video_codec.as_deref().filter(|v| !stream_absent(Some(v)));
            let audio = audio_codec.as_deref().filter(|v| !stream_absent(Some(v)));
            let codecs = match (video, audio) {
                (Some(v), Some(a)) => format!("{v} + {a}"),
                (Some(v), None) if stream_absent(audio_codec.as_deref()) => {
                    format!("{v}, yalnız video")
                }
                (None, Some(a)) if stream_absent(video_codec.as_deref()) => {
                    format!("{a}, yalnız ses")
                }
                (Some(codec), None) | (None, Some(codec)) => codec.to_string(),
                _ => String::new(),
            };
            let dynamic_range = string_field(format, "dynamic_range").or_else(|| {
                let upper = note.to_ascii_uppercase();
                ["HDR10+", "DOLBY VISION", "HDR10", "HLG", "HDR"]
                    .into_iter()
                    .find(|marker| upper.contains(marker))
                    .map(str::to_string)
            });
            let label = [
                id.as_str(),
                note,
                resolution,
                extension.as_str(),
                codecs.as_str(),
                dynamic_range.as_deref().unwrap_or(""),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            Some(MediaFormat {
                id,
                label,
                extension,
                width,
                height,
                fps,
                filesize: format
                    .get("filesize")
                    .and_then(Value::as_u64)
                    .or_else(|| format.get("filesize_approx").and_then(Value::as_u64)),
                video_codec,
                audio_codec,
                language: string_field(format, "language"),
                audio_channels: as_u32(format.get("audio_channels")),
                dynamic_range,
                has_drm: false,
            })
        })
        .collect()
}

pub(super) fn fragment_duration_tolerance(root: &Value) -> Option<f64> {
    fn visit(value: &Value, largest: &mut Option<f64>) {
        if let Some(duration) = value
            .get("fragment_duration")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            *largest = Some(largest.unwrap_or(0.0).max(duration));
        }
        if let Some(fragments) = value.get("fragments").and_then(Value::as_array) {
            for fragment in fragments {
                if let Some(duration) = fragment
                    .get("duration")
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite() && *value > 0.0)
                {
                    *largest = Some(largest.unwrap_or(0.0).max(duration));
                }
            }
        }
        for field in ["formats", "entries"] {
            if let Some(children) = value.get(field).and_then(Value::as_array) {
                for child in children {
                    visit(child, largest);
                }
            }
        }
    }
    let mut largest = None;
    visit(root, &mut largest);
    largest.map(|seconds| (seconds + 0.25).clamp(0.25, 10.0))
}

pub(super) fn explicit_drm(value: &Value) -> bool {
    match value.get("has_drm") {
        Some(Value::Bool(v)) => *v,
        Some(Value::Number(v)) => v.as_u64().unwrap_or(0) != 0,
        Some(Value::String(v)) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "" | "false" | "none" | "no" | "0"
        ),
        _ => false,
    }
}

pub(super) fn is_drm_only(value: &Value) -> bool {
    if explicit_drm(value) {
        return true;
    }
    let Some(formats) = value.get("formats").and_then(Value::as_array) else {
        return false;
    };
    let media_formats: Vec<_> = formats
        .iter()
        .filter(|format| {
            !stream_absent(format.get("vcodec").and_then(Value::as_str))
                || !stream_absent(format.get("acodec").and_then(Value::as_str))
        })
        .collect();
    !media_formats.is_empty() && media_formats.iter().all(|format| explicit_drm(format))
}

pub(super) fn live_value(value: &Value) -> bool {
    value
        .get("is_live")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || matches!(
            value.get("live_status").and_then(Value::as_str),
            Some("is_live" | "is_upcoming" | "post_live")
        )
}
pub(super) fn optional_codec(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !matches!(*v, "" | "unknown"))
        .map(str::to_string)
}
// yt-dlp uses "none" for an absent stream; null/missing means it has not identified the codec.
pub(super) fn stream_absent(value: Option<&str>) -> bool {
    value == Some("none")
}
pub(super) fn stream_present(value: Option<&str>) -> bool {
    value.is_some_and(|codec| !stream_absent(Some(codec)))
}
pub(super) fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
pub(super) fn as_u32(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}
