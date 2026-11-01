//! Output validation: ffprobe stream, duration, resolution and track checks.

use super::*;

pub(super) fn validate_probe(value: &Value, request: &AddRequest, info: &MediaInfo) -> Result<()> {
    let streams = value["streams"].as_array().ok_or_else(|| {
        anyhow!(crate::i18n::ui(
            "Medya doğrulaması: akış bulunamadı",
            "Media verification: no streams found"
        ))
    })?;
    if request.kind != DownloadKind::Audio && request.container.as_deref() == Some("mp4") {
        for stream in streams {
            let compatible = match stream["codec_type"].as_str() {
                Some("video") => stream["codec_name"].as_str().is_some_and(mp4_video_codec),
                Some("audio") => stream["codec_name"].as_str().is_some_and(mp4_audio_codec),
                _ => true,
            };
            if !compatible {
                bail!(crate::i18n::ui("Medya doğrulaması: gerçek codec MP4 uyumluluk koşulunu karşılamıyor; MKV seçin. Yeniden kodlama yapılmadı", "Media verification: the actual codec does not meet the MP4 compatibility requirement; choose MKV. No re-encoding was done"));
            }
        }
    }
    let has = |kind: &str| {
        streams.iter().any(|s| {
            s["codec_type"] == kind && s["codec_name"].as_str().is_some_and(|c| c != "unknown")
        })
    };
    if request.kind != DownloadKind::Audio && !has("video") {
        bail!(crate::i18n::ui(
            "Medya doğrulaması: görüntü akışı eksik",
            "Media verification: the video stream is missing"
        ));
    }
    if request.kind == DownloadKind::Audio && has("video") {
        bail!(crate::i18n::ui(
            "Medya doğrulaması: yalnız ses çıktısında beklenmeyen görüntü akışı var",
            "Media verification: unexpected video stream in an audio-only output"
        ));
    }
    if !has("audio")
        && (request.kind == DownloadKind::Audio
            || request.expected_audio == Some(true)
            || audio_offered(info)
            || info.formats.is_empty())
    {
        bail!(crate::i18n::ui("Medya doğrulaması: seçilen ses akışı eksik; kaynağın gerçekten sessiz olduğu doğrulanamadı. Ana HLS/DASH listesini seçin", "Media verification: the selected audio stream is missing; it could not be confirmed that the source is truly silent. Pick the main HLS/DASH playlist"));
    }
    if request.kind != DownloadKind::Audio {
        if let Some(id) = &request.video_format_id {
            let selected = info
                .formats
                .iter()
                .find(|format| format.id == *id)
                .ok_or_else(|| {
                    anyhow!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen görüntü biçimi metadata içinde yok",
                        "Media verification: the selected video format is not in the metadata"
                    ))
                })?;
            let video = streams
                .iter()
                .find(|stream| stream["codec_type"] == "video")
                .ok_or_else(|| {
                    anyhow!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen görüntü akışı çıktıda yok",
                        "Media verification: the selected video stream is not in the output"
                    ))
                })?;
            if selected
                .height
                .is_some_and(|height| video["height"].as_u64() != Some(height as u64))
            {
                bail!(crate::i18n::ui(
                    "Medya doğrulaması: seçilen görüntü yüksekliği çıktıda eşleşmiyor",
                    "Media verification: the selected video height does not match in the output"
                ));
            }
            if let (Some(expected), Some(actual)) = (
                selected.video_codec.as_deref(),
                video["codec_name"].as_str(),
            ) {
                if !codec_equivalent(expected, actual) {
                    bail!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen görüntü codec'i çıktıda eşleşmiyor",
                        "Media verification: the selected video codec does not match in the output"
                    ));
                }
            }
            if let Some(expected) = selected.fps {
                let actual = rational_value(&video["avg_frame_rate"])
                    .or_else(|| rational_value(&video["r_frame_rate"]));
                // Extractors often round the rate (60 for 60000/1001): the tolerance scales with it.
                let tolerance = (expected * 0.002).max(0.05);
                if actual.is_none_or(|actual| (actual - expected).abs() > tolerance) {
                    bail!(crate::i18n::ui("Medya doğrulaması: seçilen görüntü kare hızı çıktıda eşleşmiyor", "Media verification: the selected video frame rate does not match in the output"));
                }
            }
            if selected
                .dynamic_range
                .as_deref()
                .is_some_and(is_hdr_dynamic_range)
                && !matches!(
                    video["color_transfer"].as_str(),
                    Some("smpte2084" | "arib-std-b67")
                )
            {
                bail!(crate::i18n::ui("Medya doğrulaması: seçilen HDR/dinamik aralık çıktıda doğrulanamadı", "Media verification: the selected HDR/dynamic range could not be verified in the output"));
            }
        }
        if let Some(exact) = request.exact_height {
            if !streams
                .iter()
                .any(|s| s["codec_type"] == "video" && s["height"].as_u64() == Some(exact as u64))
            {
                bail!(crate::i18n::ui(
                    "Medya doğrulaması: seçilen kesin çözünürlük çıktıda yok",
                    "Media verification: the selected exact resolution is not in the output"
                ));
            }
        }
        if let Some(max) = request.max_height {
            if streams.iter().any(|s| {
                s["codec_type"] == "video" && s["height"].as_u64().is_some_and(|h| h > max as u64)
            }) {
                bail!(crate::i18n::ui("Medya doğrulaması: istenen çözünürlük bu kaynakta kullanılamıyor", "Media verification: the requested resolution is not available from this source"));
            }
        }
    }
    let audio_streams = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .collect::<Vec<_>>();
    if !request.audio_tracks.is_empty() {
        if audio_streams.len() != request.audio_tracks.len() {
            bail!(crate::i18n::ui(
                "Medya doğrulaması: seçilen ses parçalarının tamamı çıktıda yok",
                "Media verification: not all selected audio tracks are in the output"
            ));
        }
        for (index, (stream, selected)) in
            audio_streams.iter().zip(&request.audio_tracks).enumerate()
        {
            let selected_format = exact_audio_format(info, &selected.id)?;
            if let Some(expected) = selected_format.audio_codec.as_deref() {
                let actual = stream["codec_name"].as_str().ok_or_else(|| {
                    anyhow!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen ses codec'i çıktıda yok",
                        "Media verification: the selected audio codec is not in the output"
                    ))
                })?;
                if !codec_equivalent(expected, actual) {
                    bail!(
                        crate::i18n::ui("Medya doğrulaması: ses parçası sırası/codec'i seçilen akışla eşleşmiyor", "Media verification: the audio track order/codec does not match the selected stream")
                    );
                }
            }
            if let Some(language) = &selected.language {
                let actual = stream["tags"]["language"]
                    .as_str()
                    .filter(|value| !matches!(*value, "und" | "unknown"))
                    .ok_or_else(|| {
                        crate::error_codes::coded(
                            crate::error_codes::MED_016,
                            crate::i18n::ui("Medya doğrulaması: seçilen ses dil etiketi çıktıda yok", "Media verification: the selected audio language tag is not in the output"),
                        )
                    })?;
                if language_key(actual) != language_key(language) {
                    bail!(crate::i18n::ui("Medya doğrulaması: ses parçası sırası/dili seçilen akışla eşleşmiyor", "Media verification: the audio track order/language does not match the selected stream"));
                }
            }
            let is_default = stream["disposition"]["default"].as_i64() == Some(1);
            if is_default != (index == 0) {
                bail!(crate::i18n::ui("Medya doğrulaması: çoklu ses varsayılan akış işareti doğrulanamadı", "Media verification: the default-stream flag of the multiple audio tracks could not be verified"));
            }
        }
    } else if request.audio_format_id.is_some() || request.audio_language.is_some() {
        if audio_streams.len() != 1 {
            bail!(crate::i18n::ui(
                "Medya doğrulaması: seçilen tek ses parçası doğrulanamadı",
                "Media verification: the selected single audio track could not be verified"
            ));
        }
        let audio = audio_streams[0];
        // The exact `--format` pins the track; a converted file does not carry the source
        // track's codec or language tag.
        let converted = audio_conversion(request)?.is_some();
        if let Some(id) = request.audio_format_id.as_ref().filter(|_| !converted) {
            let selected_format = exact_audio_format(info, id)?;
            if let Some(expected) = selected_format.audio_codec.as_deref() {
                let actual = audio["codec_name"].as_str().ok_or_else(|| {
                    anyhow!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen ses codec'i çıktıda yok",
                        "Media verification: the selected audio codec is not in the output"
                    ))
                })?;
                if !codec_equivalent(expected, actual) {
                    bail!(crate::i18n::ui(
                        "Medya doğrulaması: seçilen ses codec'i çıktıda eşleşmiyor",
                        "Media verification: the selected audio codec does not match in the output"
                    ));
                }
            }
        }
        if let Some(language) = request.audio_language.as_ref().filter(|_| !converted) {
            let actual = audio["tags"]["language"]
                .as_str()
                .filter(|value| !matches!(*value, "und" | "unknown"))
                .ok_or_else(|| {
                    crate::error_codes::coded(
                        crate::error_codes::MED_016,
                        crate::i18n::ui("Medya doğrulaması: seçilen ses dil etiketi çıktıda yok", "Media verification: the selected audio language tag is not in the output"),
                    )
                })?;
            if language_key(actual) != language_key(language) {
                bail!(crate::i18n::ui(
                    "Medya doğrulaması: ses dili seçilen dille eşleşmiyor",
                    "Media verification: the audio language does not match the selected language"
                ));
            }
        }
    }

    let video_spans = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "video")
        .filter_map(stream_time_span)
        .collect::<Vec<_>>();
    let audio_spans = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .filter_map(stream_time_span)
        .collect::<Vec<_>>();
    // Content first: a missing or duplicated fragment shifts a stream's timeline, and that is the
    // reason the user has to act on. Metadata findings are reported only for intact streams.
    let start_tolerance = info.duration_tolerance.unwrap_or(0.5).clamp(0.25, 2.0);
    let end_tolerance = start_tolerance;
    // Source playlists may declare different lengths for video and audio; each stream is then
    // held to its own playlist instead of to its counterpart.
    let declared =
        !info.declared_video_durations.is_empty() && !info.declared_audio_durations.is_empty();
    if declared {
        for (kind, spans, lengths) in [
            ("video", &video_spans, &info.declared_video_durations),
            ("audio", &audio_spans, &info.declared_audio_durations),
        ] {
            for span in spans.iter() {
                let length = span.1 - span.0;
                // Whole-second segment lengths are rounded and drift from the stream by up to
                // a second every quarter hour (6.006 s segments listed as 6).
                let fits = |declared: &f64| {
                    let drift = if declared.fract() == 0.0 {
                        declared * 0.002
                    } else {
                        0.0
                    };
                    (declared - length).abs() <= end_tolerance + drift
                };
                if !lengths.iter().any(fits) {
                    let declared = lengths
                        .iter()
                        .map(|length| format!("{length:.1}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let stream_name = if kind == "video" {
                        crate::i18n::ui("görüntü", "video")
                    } else {
                        crate::i18n::ui("ses", "audio")
                    };
                    crate::bail_code!(
                        crate::error_codes::TRF_012,
                        "{}",
                        crate::i18n::ui_format!(
                            "Medya doğrulaması: {stream} akışı {length:.1} sn, kaynak listesi {declared} sn bildiriyor (eksik ya da yinelenen parça).",
                            "Media verification: the {stream} stream lasts {length:.1} s, its source playlist declares {declared} s (missing or duplicated fragment).",
                            stream = stream_name,
                            length = length,
                            declared = declared
                        )
                    );
                }
            }
        }
    }
    // Rounding drifts both playlists alike, while a lost segment moves one stream alone: with
    // whole-second lengths, the streams' distances from their own playlists have to agree.
    let rounded = declared
        && info
            .declared_video_durations
            .iter()
            .chain(&info.declared_audio_durations)
            .any(|length| length.fract() == 0.0);
    let residual = |span: (f64, f64), lengths: &[f64]| {
        lengths
            .iter()
            .map(|declared| span.1 - span.0 - declared)
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or(0.0)
    };
    if let Some(video) = video_spans.first().copied() {
        for (index, audio) in audio_spans.iter().enumerate() {
            let drift_differs = rounded
                && (residual(video, &info.declared_video_durations)
                    - residual(*audio, &info.declared_audio_durations))
                .abs()
                    > end_tolerance;
            if (video.0 - audio.0).abs() > start_tolerance
                || (!declared && (video.1 - audio.1).abs() > end_tolerance)
                || drift_differs
            {
                crate::bail_code!(
                    crate::error_codes::TRF_012,
                    "{}",
                    crate::i18n::ui_format!("Medya doğrulaması: görüntü {0:.1} sn, ses {1} {2:.1} sn; akış uzunlukları uyuşmuyor (eksik ya da yinelenen parça).", "Media verification: video {0:.1} s, audio {1} {2:.1} s; the stream lengths do not match (missing or duplicated fragment).", video.1 - video.0, index + 1, audio.1 - audio.0)
                );
            }
        }
    }

    let subtitle_streams = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "subtitle")
        .collect::<Vec<_>>();
    if !request.subtitle_tracks.is_empty() {
        if subtitle_streams.len() != request.subtitle_tracks.len() {
            bail!(crate::i18n::ui(
                "Medya doğrulaması: seçilen altyazı akışlarının tamamı çıktıda yok",
                "Media verification: not all selected subtitle streams are in the output"
            ));
        }
        for selected in &request.subtitle_tracks {
            if !subtitle_streams.iter().any(|stream| {
                stream["tags"]["language"]
                    .as_str()
                    .is_some_and(|actual| language_key(actual) == language_key(&selected.language))
            }) {
                bail!(
                    "{}",
                    crate::i18n::ui_format!(
                        "Medya doğrulaması: seçilen altyazı çıktıda yok: {0}",
                        "Media verification: the selected subtitle is not in the output: {0}",
                        selected.language
                    )
                );
            }
        }
    }
    if !request.external_subtitles.is_empty() {
        if subtitle_streams.len() != request.external_subtitles.len() {
            bail!(crate::i18n::ui(
                "Medya doğrulaması: seçilen harici altyazıların tamamı çıktıda yok",
                "Media verification: not all selected external subtitles are in the output"
            ));
        }
        // An MP4 with no default subtitle enables its first one, which reads back as default.
        // The file's own format decides: a request without a container keeps the source's.
        let first_enabled = value["format"]["format_name"]
            .as_str()
            .is_some_and(|name| name.split(',').any(|format| format == "mp4"))
            && !request
                .external_subtitles
                .iter()
                .any(|subtitle| subtitle.is_default);
        for (index, selected) in request.external_subtitles.iter().enumerate() {
            let stream = subtitle_streams[index];
            // Matroska stores no tag for an undetermined language.
            let language_matches = match stream["tags"]["language"].as_str() {
                Some(actual) => language_key(actual) == language_key(&selected.language),
                None => language_key(&selected.language) == language_key("und"),
            };
            if !language_matches {
                bail!(crate::i18n::ui("Medya doğrulaması: harici altyazı dili/sırası seçilen track ile eşleşmiyor", "Media verification: the external subtitle language/order does not match the selected track"));
            }
            let is_default = stream["disposition"]["default"].as_i64() == Some(1);
            if is_default != (selected.is_default || (first_enabled && index == 0)) {
                bail!(crate::i18n::ui(
                    "Medya doğrulaması: harici altyazı varsayılan işareti eşleşmiyor",
                    "Media verification: the external subtitle default flag does not match"
                ));
            }
        }
    }
    // Legacy CLI subtitle patterns keep yt-dlp semantics; the popup sends exact codes.
    for language in request.subtitle_languages.iter().filter(|language| {
        request.subtitle_mode.is_some()
            || !(language.as_str() == "all" || language.contains(['*', '.']))
    }) {
        if !subtitle_streams.iter().any(|s| {
            s["tags"]["language"]
                .as_str()
                .is_some_and(|l| language_key(l) == language_key(language))
        }) {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Medya doğrulaması: seçilen altyazı çıktıda yok: {language}",
                    "Media verification: the selected subtitle is not in the output: {language}",
                    language = language
                )
            );
        }
    }

    let output_span = output_time_span(value);
    if !request.playlist && !info.is_live {
        if let Some(expected) = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        {
            let actual = output_span
                .map(|(start, end)| end - start)
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| anyhow!(crate::i18n::ui("Medya doğrulaması: kaynak süresi biliniyor ancak çıktı zaman damgaları okunamadı", "Media verification: the source duration is known but the output timestamps could not be read")))?;
            let tolerance = info.duration_tolerance.unwrap_or(0.5).clamp(0.25, 10.0);
            // Extractors can truncate source duration to whole seconds. That
            // uncertainty extends the upper bound only; it must never excuse
            // missing media below the declared duration. Explicit fragment
            // timing already supplies its own bounded tolerance.
            let quantization = if info.duration_tolerance.is_none() && expected.fract() == 0.0 {
                1.0
            } else {
                0.0
            };
            if actual < expected - tolerance || actual > expected + tolerance + quantization {
                bail!(
                    "{}",
                    crate::i18n::ui_format!("Medya doğrulaması: çıktı süresi kaynakla eşleşmiyor (erken bitiş toleransı {tolerance:.3} sn; geç bitiş {0:.3} sn)", "Media verification: the output duration does not match the source (early-end tolerance {tolerance:.3} s; late end {0:.3} s)", tolerance + quantization, tolerance = tolerance)
                );
            }
        }
    }
    Ok(())
}

/// Start and end of the output: the container's, else those of its audio and video streams.
fn output_time_span(probe: &Value) -> Option<(f64, f64)> {
    format_time_span(&probe["format"]).or_else(|| {
        probe["streams"]
            .as_array()?
            .iter()
            .filter(|stream| matches!(stream["codec_type"].as_str(), Some("video" | "audio")))
            .filter_map(stream_time_span)
            .reduce(|left, right| (left.0.min(right.0), left.1.max(right.1)))
    })
}

pub(super) fn rational_value(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    let text = value.as_str()?;
    if let Some((numerator, denominator)) = text.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        return (denominator != 0.0).then_some(numerator / denominator);
    }
    text.parse::<f64>().ok().filter(|number| number.is_finite())
}

pub(super) fn codec_equivalent(expected: &str, actual: &str) -> bool {
    fn family(value: &str) -> &str {
        let value = value.split('.').next().unwrap_or(value);
        match value.to_ascii_lowercase().as_str() {
            "avc1" | "avc" | "h264" => "h264",
            "hev1" | "hvc1" | "dvh1" | "dvhe" | "hevc" | "h265" => "hevc",
            "av01" | "av1" => "av1",
            "vp09" | "vp9" => "vp9",
            "mp4v" | "mpeg4" => "mpeg4",
            "mp4a" | "aac" => "aac",
            "opus" => "opus",
            "vorbis" => "vorbis",
            "mp3" => "mp3",
            "flac" => "flac",
            "alac" => "alac",
            "ac3" | "ac-3" => "ac3",
            "eac3" | "ec-3" => "eac3",
            _ => "",
        }
    }
    let expected_family = family(expected);
    !expected_family.is_empty() && expected_family == family(actual)
}

pub(super) fn numeric_seconds(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite())
}

pub(super) fn time_base_seconds(value: &Value) -> Option<f64> {
    let (numerator, denominator) = value.as_str()?.split_once('/')?;
    let numerator = numerator.parse::<f64>().ok()?;
    let denominator = denominator.parse::<f64>().ok()?;
    (denominator != 0.0).then_some(numerator / denominator)
}

/// Start and end of a stream. A Matroska stream without a duration of its own has the tag
/// `DURATION`, which FFmpeg writes as the end time of the stream, not its length.
pub(super) fn stream_time_span(stream: &Value) -> Option<(f64, f64)> {
    let time_base = time_base_seconds(&stream["time_base"]);
    let start = numeric_seconds(&stream["start_time"])
        .or_else(|| Some(numeric_seconds(&stream["start_pts"])? * time_base?))
        .unwrap_or(0.0);
    let duration = numeric_seconds(&stream["duration"])
        .or_else(|| Some(numeric_seconds(&stream["duration_ts"])? * time_base?));
    match duration {
        Some(duration) => (duration > 0.0).then_some((start, start + duration)),
        None => {
            let end = parse_clock_timestamp(stream["tags"]["DURATION"].as_str()?)?;
            (end > start).then_some((start, end))
        }
    }
}

pub(super) fn format_time_span(format: &Value) -> Option<(f64, f64)> {
    let start = numeric_seconds(&format["start_time"]).unwrap_or(0.0);
    let duration = numeric_seconds(&format["duration"])?;
    (duration > 0.0).then_some((start, start + duration))
}

pub(super) fn parse_clock_timestamp(value: &str) -> Option<f64> {
    let mut fields = value.trim().split(':').collect::<Vec<_>>();
    let seconds = fields.pop()?.replace(',', ".").parse::<f64>().ok()?;
    let minutes = fields.pop().unwrap_or("0").parse::<f64>().ok()?;
    let hours = fields.pop().unwrap_or("0").parse::<f64>().ok()?;
    (fields.is_empty() && seconds.is_finite() && minutes >= 0.0 && hours >= 0.0)
        .then_some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Whether FFmpeg reported a problem reading or decoding the file. The null muxer's own
/// complaints about the decoded frames' timestamps are left out: an encoder that writes its
/// last frame twice with one timestamp leaves a file that decodes and plays in full.
fn decode_errors(stderr: &str) -> bool {
    let mut muxer_line = false;
    stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .any(|line| {
            muxer_line = line.starts_with("[null @ ")
                || (muxer_line && line.trim_start().starts_with("Last message repeated"));
            !muxer_line
        })
}

/// A download whose last segment ends inside a frame plays, but that frame cannot be decoded.
/// Where the source confirms a stream, decoding stops this long before the end of the stream
/// (see `confirming_duration`).
const END_MARGIN: f64 = 0.5;

/// A stream with less than this left to decode in a run is not decoded in it.
const MIN_DECODE: f64 = 0.1;

/// The source's duration, where it can confirm that a stream is complete.
///
/// A download that is cut short ends with a frame that cannot be decoded, just like a source whose
/// last segment ends inside a frame, and the cut is always in the last moments of the stream. The
/// decoder cannot tell the two apart, so the end of a stream is left out only where the source's
/// duration confirms that stream (see `decoded_streams`). It confirms nothing for a playlist or a
/// live recording, and nothing where an extractor may have cut the duration to whole seconds: the
/// real length can then be a second more, and the margin could hide more than `END_MARGIN`.
pub(super) fn confirming_duration(request: &AddRequest, info: &MediaInfo) -> Option<f64> {
    let expected = info
        .duration
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)?;
    // The condition under which `validate_probe` allows a second more than the duration.
    let exact = info.duration_tolerance.is_some() || expected.fract() != 0.0;
    (!request.playlist && !info.is_live && exact).then_some(expected)
}

/// An audio or video stream of the output.
pub(super) struct DecodedStream {
    pub(super) index: u64,
    /// Where decoding of the stream stops on the file's timeline: `END_MARGIN` before its own end
    /// where the source confirms the stream, and else at its end (`None`).
    pub(super) stop: Option<f64>,
}

/// The streams to decode. A stream is confirmed when it is longer than twice the margin and ends
/// at most `END_MARGIN` before the source's duration; one that ends earlier, such as the audio of
/// a file whose video is complete, may be cut short and is decoded to its end. Ends are measured
/// from the earliest audio or video start, where FFmpeg starts the timeline of the decode.
pub(super) fn decoded_streams(probe: &Value, confirmed: Option<f64>) -> Vec<DecodedStream> {
    let streams = probe["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|stream| matches!(stream["codec_type"].as_str(), Some("video" | "audio")))
        .filter_map(|stream| Some((stream["index"].as_u64()?, stream_time_span(stream))))
        .collect::<Vec<_>>();
    let start = streams
        .iter()
        .filter_map(|(_, span)| span.map(|(start, _)| start))
        .reduce(f64::min)
        .unwrap_or(0.0);
    streams
        .into_iter()
        .map(|(index, span)| {
            let stop = span.zip(confirmed).and_then(|((_, end), expected)| {
                let end = end - start;
                (end > 2.0 * END_MARGIN && end >= expected - END_MARGIN).then_some(end - END_MARGIN)
            });
            DecodedStream { index, stop }
        })
        .collect()
}

/// The mapping and output arguments of an FFmpeg decode run over `window` seconds (`None`: to the
/// end) from `from` on the file's timeline. Streams that are decoded to their end go to one
/// output. Otherwise every stream has an output of its own, which stops at the stream's `stop`.
/// Empty when no stream has enough left to decode.
pub(super) fn decode_outputs(
    streams: &[DecodedStream],
    from: f64,
    window: Option<f64>,
) -> Vec<String> {
    let limit = |length: Option<f64>| -> Vec<String> {
        length
            .map(|seconds| vec!["-t".to_string(), format!("{seconds:.3}")])
            .unwrap_or_default()
    };
    if streams.iter().all(|stream| stream.stop.is_none()) {
        let mut args = limit(window);
        args.extend(["-map", "0:v?", "-map", "0:a?", "-f", "null", "-"].map(String::from));
        return args;
    }
    let mut args = Vec::new();
    for stream in streams {
        let to_stop = stream.stop.map(|stop| stop - from);
        let length = match (to_stop, window) {
            (Some(to_stop), Some(window)) => Some(to_stop.min(window)),
            (to_stop, window) => to_stop.or(window),
        };
        if length.is_some_and(|seconds| seconds < MIN_DECODE) {
            continue;
        }
        args.extend(limit(length));
        args.extend(["-map".to_string(), format!("0:{}", stream.index)]);
        args.extend(["-f", "null", "-"].map(String::from));
    }
    args
}

pub(super) fn verify_media_output(
    path: &Path,
    request: &AddRequest,
    info: &MediaInfo,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
) -> Result<()> {
    let result = run_capture(
        &tools.ffprobe,
        &[
            "-v".into(),
            "error".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-show_streams".into(),
            "-show_format".into(),
            "-of".into(),
            "json".into(),
            path.to_string_lossy().into_owned(),
        ],
        b"",
        Duration::from_secs(60),
        control,
    )?;
    if !result.status.success() {
        bail!(crate::i18n::ui(
            "Medya doğrulaması: çıktı okunamadı",
            "Media verification: the output could not be read"
        ));
    }
    let probe: Value = serde_json::from_slice(&result.stdout)?;
    validate_probe(&probe, request, info)?;

    let span = output_time_span(&probe);
    let duration = span.map(|(start, end)| end - start);
    let streams = decoded_streams(&probe, confirming_duration(request, info));
    if request.full_verification {
        let mut args: Vec<String> = [
            "-v",
            "error",
            "-xerror",
            "-nostdin",
            "-protocol_whitelist",
            "file,pipe",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.extend(["-i".into(), path.to_string_lossy().into_owned()]);
        args.extend(decode_outputs(&streams, 0.0, None));
        let decoded = run_capture(
            &tools.ffmpeg,
            &args,
            b"",
            Duration::from_secs(24 * 60 * 60),
            control,
        )?;
        if !decoded.status.success() || decode_errors(&decoded.stderr) {
            bail!(crate::i18n::ui(
                "Tam medya doğrulaması: ses/görüntü baştan sona çözülemedi",
                "Full media verification: the audio/video could not be decoded from start to end"
            ));
        }
        return Ok(());
    }

    // Start, two bounded middle positions and end are decoded. This deliberately
    // reports bounded sampling rather than claiming a full-file integrity pass.
    let mut samples = vec![None];
    if let Some(duration) = duration.filter(|seconds| *seconds > 6.0) {
        samples.push(Some(duration / 3.0));
        samples.push(Some(duration * 2.0 / 3.0));
    }
    if duration.is_none_or(|seconds| seconds > 2.0) {
        samples.push(Some(-2.0));
    }
    for offset in samples {
        let from = match offset {
            Some(seconds) if seconds < 0.0 => duration.map_or(0.0, |duration| duration + seconds),
            Some(seconds) => seconds,
            None => 0.0,
        };
        let outputs = decode_outputs(&streams, from, Some(2.0));
        if outputs.is_empty() {
            continue;
        }
        let mut args: Vec<String> = ["-v", "error", "-xerror", "-nostdin"]
            .into_iter()
            .map(str::to_string)
            .collect();
        if let Some(seconds) = offset {
            if seconds < 0.0 {
                args.extend(["-sseof".into(), seconds.to_string()]);
            } else {
                args.extend(["-ss".into(), format!("{seconds:.3}")]);
            }
        }
        args.extend([
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-i".into(),
            path.to_string_lossy().into_owned(),
        ]);
        args.extend(outputs);
        let decoded = run_capture(&tools.ffmpeg, &args, b"", Duration::from_secs(60), control)?;
        if !decoded.status.success() || decode_errors(&decoded.stderr) {
            bail!(crate::i18n::ui(
                "Medya doğrulaması: örneklenen başlangıç/orta/bitiş bölümü çözülemedi",
                "Media verification: the sampled start/middle/end section could not be decoded"
            ));
        }
    }
    Ok(())
}
