//! Output validation: ffprobe stream, duration, resolution and track checks.

use super::*;

pub(super) fn validate_probe(value: &Value, request: &AddRequest, info: &MediaInfo) -> Result<()> {
    let streams = value["streams"]
        .as_array()
        .ok_or_else(|| anyhow!("Medya doğrulaması: akış bulunamadı"))?;
    if request.kind != DownloadKind::Audio && request.container.as_deref() == Some("mp4") {
        for stream in streams {
            let compatible = match stream["codec_type"].as_str() {
                Some("video") => stream["codec_name"].as_str().is_some_and(mp4_video_codec),
                Some("audio") => stream["codec_name"].as_str().is_some_and(mp4_audio_codec),
                _ => true,
            };
            if !compatible {
                bail!("Medya doğrulaması: gerçek codec MP4 uyumluluk koşulunu karşılamıyor; MKV seçin. Yeniden kodlama yapılmadı");
            }
        }
    }
    let has = |kind: &str| {
        streams.iter().any(|s| {
            s["codec_type"] == kind && s["codec_name"].as_str().is_some_and(|c| c != "unknown")
        })
    };
    if request.kind != DownloadKind::Audio && !has("video") {
        bail!("Medya doğrulaması: görüntü akışı eksik");
    }
    if request.kind == DownloadKind::Audio && has("video") {
        bail!("Medya doğrulaması: yalnız ses çıktısında beklenmeyen görüntü akışı var");
    }
    if !has("audio")
        && (request.kind == DownloadKind::Audio
            || request.expected_audio == Some(true)
            || audio_offered(info)
            || info.formats.is_empty())
    {
        bail!("Medya doğrulaması: seçilen ses akışı eksik; kaynağın gerçekten sessiz olduğu doğrulanamadı. Ana HLS/DASH listesini seçin");
    }
    if request.kind != DownloadKind::Audio {
        if let Some(id) = &request.video_format_id {
            let selected = info
                .formats
                .iter()
                .find(|format| format.id == *id)
                .ok_or_else(|| {
                    anyhow!("Medya doğrulaması: seçilen görüntü biçimi metadata içinde yok")
                })?;
            let video = streams
                .iter()
                .find(|stream| stream["codec_type"] == "video")
                .ok_or_else(|| anyhow!("Medya doğrulaması: seçilen görüntü akışı çıktıda yok"))?;
            if selected
                .height
                .is_some_and(|height| video["height"].as_u64() != Some(height as u64))
            {
                bail!("Medya doğrulaması: seçilen görüntü yüksekliği çıktıda eşleşmiyor");
            }
            if let (Some(expected), Some(actual)) = (
                selected.video_codec.as_deref(),
                video["codec_name"].as_str(),
            ) {
                if !codec_equivalent(expected, actual) {
                    bail!("Medya doğrulaması: seçilen görüntü codec'i çıktıda eşleşmiyor");
                }
            }
            if let Some(expected) = selected.fps {
                let actual = rational_value(&video["avg_frame_rate"])
                    .or_else(|| rational_value(&video["r_frame_rate"]));
                if actual.is_none_or(|actual| (actual - expected).abs() > 0.05) {
                    bail!("Medya doğrulaması: seçilen görüntü kare hızı çıktıda eşleşmiyor");
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
                bail!("Medya doğrulaması: seçilen HDR/dinamik aralık çıktıda doğrulanamadı");
            }
        }
        if let Some(exact) = request.exact_height {
            if !streams
                .iter()
                .any(|s| s["codec_type"] == "video" && s["height"].as_u64() == Some(exact as u64))
            {
                bail!("Medya doğrulaması: seçilen kesin çözünürlük çıktıda yok");
            }
        }
        if let Some(max) = request.max_height {
            if streams.iter().any(|s| {
                s["codec_type"] == "video" && s["height"].as_u64().is_some_and(|h| h > max as u64)
            }) {
                bail!("Medya doğrulaması: istenen çözünürlük bu kaynakta kullanılamıyor");
            }
        }
    }
    let audio_streams = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .collect::<Vec<_>>();
    if !request.audio_tracks.is_empty() {
        if audio_streams.len() != request.audio_tracks.len() {
            bail!("Medya doğrulaması: seçilen ses parçalarının tamamı çıktıda yok");
        }
        for (index, (stream, selected)) in
            audio_streams.iter().zip(&request.audio_tracks).enumerate()
        {
            let selected_format = exact_audio_format(info, &selected.id)?;
            if let Some(expected) = selected_format.audio_codec.as_deref() {
                let actual = stream["codec_name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Medya doğrulaması: seçilen ses codec'i çıktıda yok"))?;
                if !codec_equivalent(expected, actual) {
                    bail!(
                        "Medya doğrulaması: ses parçası sırası/codec'i seçilen akışla eşleşmiyor"
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
                            "Medya doğrulaması: seçilen ses dil etiketi çıktıda yok",
                        )
                    })?;
                if language_key(actual) != language_key(language) {
                    bail!("Medya doğrulaması: ses parçası sırası/dili seçilen akışla eşleşmiyor");
                }
            }
            let is_default = stream["disposition"]["default"].as_i64() == Some(1);
            if is_default != (index == 0) {
                bail!("Medya doğrulaması: çoklu ses varsayılan akış işareti doğrulanamadı");
            }
        }
    } else if request.audio_format_id.is_some() || request.audio_language.is_some() {
        if audio_streams.len() != 1 {
            bail!("Medya doğrulaması: seçilen tek ses parçası doğrulanamadı");
        }
        let audio = audio_streams[0];
        if let Some(id) = &request.audio_format_id {
            let selected_format = exact_audio_format(info, id)?;
            if let Some(expected) = selected_format.audio_codec.as_deref() {
                let actual = audio["codec_name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Medya doğrulaması: seçilen ses codec'i çıktıda yok"))?;
                if !codec_equivalent(expected, actual) {
                    bail!("Medya doğrulaması: seçilen ses codec'i çıktıda eşleşmiyor");
                }
            }
        }
        if let Some(language) = &request.audio_language {
            let actual = audio["tags"]["language"]
                .as_str()
                .filter(|value| !matches!(*value, "und" | "unknown"))
                .ok_or_else(|| {
                    crate::error_codes::coded(
                        crate::error_codes::MED_016,
                        "Medya doğrulaması: seçilen ses dil etiketi çıktıda yok",
                    )
                })?;
            if language_key(actual) != language_key(language) {
                bail!("Medya doğrulaması: ses dili seçilen dille eşleşmiyor");
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
    if let Some(video) = video_spans.first().copied() {
        for (index, audio) in audio_spans.iter().enumerate() {
            if (video.0 - audio.0).abs() > start_tolerance
                || (video.1 - audio.1).abs() > end_tolerance
            {
                crate::bail_code!(
                    crate::error_codes::TRF_012,
                    "Medya doğrulaması: görüntü {:.1} sn, ses {} {:.1} sn; akış uzunlukları uyuşmuyor (eksik ya da yinelenen parça).",
                    video.1 - video.0,
                    index + 1,
                    audio.1 - audio.0
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
            bail!("Medya doğrulaması: seçilen altyazı akışlarının tamamı çıktıda yok");
        }
        for selected in &request.subtitle_tracks {
            if !subtitle_streams.iter().any(|stream| {
                stream["tags"]["language"]
                    .as_str()
                    .is_some_and(|actual| language_key(actual) == language_key(&selected.language))
            }) {
                bail!(
                    "Medya doğrulaması: seçilen altyazı çıktıda yok: {}",
                    selected.language
                );
            }
        }
    }
    if !request.external_subtitles.is_empty() {
        if subtitle_streams.len() != request.external_subtitles.len() {
            bail!("Medya doğrulaması: seçilen harici altyazıların tamamı çıktıda yok");
        }
        for (index, selected) in request.external_subtitles.iter().enumerate() {
            let stream = subtitle_streams[index];
            if stream["tags"]["language"]
                .as_str()
                .is_none_or(|actual| language_key(actual) != language_key(&selected.language))
            {
                bail!("Medya doğrulaması: harici altyazı dili/sırası seçilen track ile eşleşmiyor");
            }
            let is_default = stream["disposition"]["default"].as_i64() == Some(1);
            if is_default != selected.is_default {
                bail!("Medya doğrulaması: harici altyazı varsayılan işareti eşleşmiyor");
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
            bail!("Medya doğrulaması: seçilen altyazı çıktıda yok: {language}");
        }
    }

    let output_span = format_time_span(&value["format"]).or_else(|| {
        video_spans
            .iter()
            .copied()
            .chain(
                streams
                    .iter()
                    .filter(|stream| stream["codec_type"] == "audio")
                    .filter_map(stream_time_span),
            )
            .reduce(|left, right| (left.0.min(right.0), left.1.max(right.1)))
    });
    if !request.playlist && !info.is_live {
        if let Some(expected) = info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
        {
            let actual = output_span
                .map(|(start, end)| end - start)
                .filter(|duration| duration.is_finite() && *duration > 0.0)
                .ok_or_else(|| anyhow!("Medya doğrulaması: kaynak süresi biliniyor ancak çıktı zaman damgaları okunamadı"))?;
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
                bail!("Medya doğrulaması: çıktı süresi kaynakla eşleşmiyor (erken bitiş toleransı {tolerance:.3} sn; geç bitiş {:.3} sn)", tolerance + quantization);
            }
        }
    }
    Ok(())
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
            "hev1" | "hvc1" | "hevc" | "h265" => "hevc",
            "av01" | "av1" => "av1",
            "vp09" | "vp9" => "vp9",
            "mp4v" | "mpeg4" => "mpeg4",
            "mp4a" | "aac" => "aac",
            "opus" => "opus",
            "vorbis" => "vorbis",
            "mp3" => "mp3",
            "flac" => "flac",
            "alac" => "alac",
            "ac3" => "ac3",
            "eac3" => "eac3",
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

pub(super) fn stream_time_span(stream: &Value) -> Option<(f64, f64)> {
    let time_base = time_base_seconds(&stream["time_base"]);
    let start = numeric_seconds(&stream["start_time"])
        .or_else(|| Some(numeric_seconds(&stream["start_pts"])? * time_base?))
        .unwrap_or(0.0);
    let duration = numeric_seconds(&stream["duration"])
        .or_else(|| Some(numeric_seconds(&stream["duration_ts"])? * time_base?))
        .or_else(|| parse_clock_timestamp(stream["tags"]["DURATION"].as_str()?))?;
    (duration > 0.0).then_some((start, start + duration))
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
        bail!("Medya doğrulaması: çıktı okunamadı");
    }
    let probe: Value = serde_json::from_slice(&result.stdout)?;
    validate_probe(&probe, request, info)?;

    if request.full_verification {
        let args = vec![
            "-v".into(),
            "error".into(),
            "-xerror".into(),
            "-nostdin".into(),
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-i".into(),
            path.to_string_lossy().into_owned(),
            "-map".into(),
            "0:v?".into(),
            "-map".into(),
            "0:a?".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        let decoded = run_capture(
            &tools.ffmpeg,
            &args,
            b"",
            Duration::from_secs(24 * 60 * 60),
            control,
        )?;
        if !decoded.status.success() || !decoded.stderr.trim().is_empty() {
            bail!("Tam medya doğrulaması: ses/görüntü baştan sona çözülemedi");
        }
        return Ok(());
    }

    // Start, two bounded middle positions and end are decoded. This deliberately
    // reports bounded sampling rather than claiming a full-file integrity pass.
    let duration = format_time_span(&probe["format"])
        .map(|(start, end)| end - start)
        .or_else(|| {
            probe["streams"]
                .as_array()?
                .iter()
                .filter_map(stream_time_span)
                .map(|(start, end)| end - start)
                .reduce(f64::max)
        });
    let mut samples = vec![None];
    if let Some(duration) = duration.filter(|seconds| *seconds > 6.0) {
        samples.push(Some(duration / 3.0));
        samples.push(Some(duration * 2.0 / 3.0));
    }
    if duration.is_none_or(|seconds| seconds > 2.0) {
        samples.push(Some(-2.0));
    }
    for offset in samples {
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
            "-t".into(),
            "2".into(),
            "-map".into(),
            "0:v?".into(),
            "-map".into(),
            "0:a?".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ]);
        let decoded = run_capture(&tools.ffmpeg, &args, b"", Duration::from_secs(60), control)?;
        if !decoded.status.success() || !decoded.stderr.trim().is_empty() {
            bail!("Medya doğrulaması: örneklenen başlangıç/orta/bitiş bölümü çözülemedi");
        }
    }
    Ok(())
}
