//! Post-processing of selected streams: muxing extra tracks and subtitle conversion.

use super::*;

pub(super) fn postprocess_selected_streams(
    path: &Path,
    request: &AddRequest,
    info: &MediaInfo,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<bool> {
    let single_audio = request.audio_format_id.is_some() || request.audio_language.is_some();
    if request.audio_tracks.is_empty() && !single_audio && request.external_subtitles.is_empty() {
        return Ok(false);
    }
    let audio_count = if request.audio_tracks.is_empty() {
        usize::from(single_audio)
    } else {
        request.audio_tracks.len()
    };
    let audio_language = |index: usize| {
        request
            .audio_tracks
            .get(index)
            .and_then(|selected| selected.language.as_deref())
            .or_else(|| {
                (request.audio_tracks.is_empty() && index == 0)
                    .then_some(request.audio_language.as_deref())
                    .flatten()
            })
    };
    if audio_count > 0 {
        let probe = run_capture(
            &tools.ffprobe,
            &[
                "-v".into(),
                "error".into(),
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-show_streams".into(),
                "-of".into(),
                "json".into(),
                path.to_string_lossy().into_owned(),
            ],
            b"",
            Duration::from_secs(60),
            control,
        )?;
        if !probe.status.success() {
            bail!("Seçilen ses akışları etiketlenmeden doğrulanamadı");
        }
        let metadata: Value = serde_json::from_slice(&probe.stdout)?;
        let audio = metadata["streams"]
            .as_array()
            .context("Ses akış bilgisi yok")?
            .iter()
            .filter(|stream| stream["codec_type"] == "audio")
            .collect::<Vec<_>>();
        if audio.len() != audio_count {
            bail!("Seçilen ses akış sayısı çıktı ile eşleşmiyor");
        }
        for (index, stream) in audio.iter().enumerate() {
            if let (Some(expected), Some(actual)) = (
                audio_language(index),
                stream["tags"]["language"]
                    .as_str()
                    .filter(|value| !matches!(*value, "und" | "unknown" | "")),
            ) {
                if language_key(expected) != language_key(actual) {
                    bail!("Ses çıktısının mevcut dil etiketi seçilen dille çelişiyor; etiket değiştirilmedi");
                }
            }
        }
    }
    let parent = path.parent().context("Medya çalışma klasörü eksik")?;
    let mut subtitles = Vec::with_capacity(request.external_subtitles.len());
    for (index, selected) in request.external_subtitles.iter().enumerate() {
        let subtitle_path = parent.join(format!("external-subtitle-{index}.txt"));
        download_external_subtitle(
            selected,
            &request.session_cookies,
            request.page_url.as_deref(),
            &subtitle_path,
            control,
            network,
        )?;
        validate_external_subtitle_file(
            &subtitle_path,
            selected,
            info.duration,
            info.duration_tolerance,
        )?;
        subtitles.push(subtitle_path);
    }

    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .context("Medya dosya türü eksik")?;
    let staged = parent.join(format!("selected-output.{extension}"));
    if staged.exists() {
        fs::remove_file(&staged)?;
    }
    let mut args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-nostdin".into(),
        "-protocol_whitelist".into(),
        "file,pipe".into(),
        "-i".into(),
        path.to_string_lossy().into_owned(),
    ];
    for subtitle in &subtitles {
        args.extend(["-i".into(), subtitle.to_string_lossy().into_owned()]);
    }
    args.extend(["-map".into(), "0:v?".into(), "-map".into(), "0:a?".into()]);
    if subtitles.is_empty() {
        args.extend(["-map".into(), "0:s?".into()]);
    }
    for index in 0..subtitles.len() {
        args.extend(["-map".into(), format!("{}:0", index + 1)]);
    }
    args.extend([
        "-map_metadata".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
    ]);
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
    for index in 0..audio_count {
        if let Some(language) = audio_language(index) {
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
    for (index, selected) in request.external_subtitles.iter().enumerate() {
        args.extend([
            format!("-metadata:s:s:{index}"),
            format!(
                "language={}",
                tag_language(&selected.language, request.container.as_deref())
            ),
            format!("-metadata:s:s:{index}"),
            format!("title={}", selected.label),
            format!("-disposition:s:{index}"),
            if selected.is_default { "default" } else { "0" }.into(),
        ]);
    }
    args.push(staged.to_string_lossy().into_owned());
    let muxed = run_capture(
        &tools.ffmpeg,
        &args,
        b"",
        Duration::from_secs(10 * 60),
        control,
    )?;
    if !muxed.status.success() || !muxed.stderr.trim().is_empty() {
        bail!(
            "Seçilen ses/altyazı akışları kayıpsız birleştirilemedi: {}",
            bounded_tail(&muxed.stderr, 4096)
        );
    }
    verify_media_output(&staged, request, info, tools, control)?;
    let backup = parent.join(format!("downloaded-before-selection.{extension}"));
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    fs::rename(path, &backup)?;
    if let Err(error) = fs::rename(&staged, path) {
        let _ = fs::rename(&backup, path);
        return Err(error).context("Seçilen akış çıktısı çalışma dosyasına alınamadı");
    }
    fs::remove_file(backup)?;
    for subtitle in subtitles {
        fs::remove_file(subtitle)?;
    }
    Ok(true)
}

pub(super) fn download_external_subtitle(
    selected: &ExternalSubtitle,
    cookies: &[ScopedCookie],
    page_url: Option<&str>,
    path: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<()> {
    const MAX_SUBTITLE_BYTES: usize = 8 * 1024 * 1024;
    let target = Url::parse(&selected.url).context("Harici altyazı adresi geçersiz")?;
    // The cookies belong to the handoff page, not to the selected subtitle resource: its own
    // Referer stays a network observation only.
    validate_scoped_cookies(cookies, page_url)?;
    let applicable_cookies = cookies
        .iter()
        .filter(|cookie| scoped_cookie_applies(cookie, &target))
        .collect::<Vec<_>>();
    let cookie_jar = if applicable_cookies.is_empty() {
        None
    } else {
        Some(TemporaryCookieJar(write_cookie_jar(applicable_cookies)?))
    };
    let mut easy = network.easy_for_url(&target, control)?;
    easy.url(&selected.url)?;
    easy.useragent("SSDownload/1.3")?;
    if let Some(cookie_jar) = &cookie_jar {
        easy.cookie_file(&cookie_jar.0)?;
    }
    easy.follow_location(false)?;
    easy.fail_on_error(false)?;
    easy.connect_timeout(Duration::from_secs(20))?;
    easy.timeout(Duration::from_secs(60))?;
    easy.progress(true)?;
    if let Some(referer) = &selected.referer {
        easy.referer(referer)?;
    }
    let mut headers = List::new();
    for (name, value) in &selected.headers {
        headers.append(&format!("{name}: {value}"))?;
    }
    easy.http_headers(headers)?;
    let mut file = File::create(path)?;
    let mut received = 0usize;
    let mut oversized = false;
    let mut write_failed = false;
    let performed = {
        let mut transfer = easy.transfer();
        transfer.progress_function(|_, _, _, _| !control.stop_requested())?;
        transfer.write_function(|data| {
            if received.saturating_add(data.len()) > MAX_SUBTITLE_BYTES {
                oversized = true;
                return Ok(0);
            }
            if file.write_all(data).is_err() {
                write_failed = true;
                return Ok(0);
            }
            received += data.len();
            Ok(data.len())
        })?;
        transfer.perform()
    };
    if let Err(error) = performed {
        let _ = fs::remove_file(path);
        if control.stop_requested() {
            bail!("Harici altyazı indirmesi durduruldu");
        }
        if oversized {
            bail!("Harici altyazı güvenlik boyutu sınırını aştı");
        }
        if write_failed {
            bail!("Harici altyazı çalışma dosyasına yazılamadı");
        }
        return Err(error).context("Harici altyazı indirilemedi");
    }
    file.flush()?;
    let status = easy.response_code()?;
    if (300..400).contains(&status) {
        fs::remove_file(path)?;
        bail!("Harici altyazı başka sunucuya yönlendirildi; hedefe özel başlıklar güvenle aktarılamadı");
    }
    if !(200..300).contains(&status) {
        fs::remove_file(path)?;
        bail!("Harici altyazı sunucusu HTTP {status} döndürdü");
    }
    if received == 0 {
        fs::remove_file(path)?;
        bail!("Harici altyazı boş döndü");
    }
    Ok(())
}

pub(super) fn validate_external_subtitle_file(
    path: &Path,
    selected: &ExternalSubtitle,
    source_duration: Option<f64>,
    source_tolerance: Option<f64>,
) -> Result<()> {
    let bytes = fs::read(path)?;
    let text = std::str::from_utf8(&bytes).context("Harici altyazı UTF-8 değil")?;
    let is_vtt = text.trim_start_matches('\u{feff}').starts_with("WEBVTT");
    let mut cues = 0usize;
    let mut last_end = 0.0f64;
    for line in text.lines().filter(|line| line.contains("-->")) {
        let (start, end) = line
            .split_once("-->")
            .context("Harici altyazı zaman satırı geçersiz")?;
        let start = parse_clock_timestamp(start.trim())
            .ok_or_else(|| anyhow!("Harici altyazı başlangıç zamanı geçersiz"))?;
        let end_token = end.split_ascii_whitespace().next().unwrap_or("");
        let end = parse_clock_timestamp(end_token)
            .ok_or_else(|| anyhow!("Harici altyazı bitiş zamanı geçersiz"))?;
        if start < 0.0 || end <= start {
            bail!("Harici altyazı zaman aralığı geçersiz");
        }
        cues += 1;
        last_end = last_end.max(end);
    }
    if cues == 0 || (!is_vtt && !looks_like_srt(text)) {
        bail!("Harici track geçerli VTT veya SRT altyazısı değil");
    }
    if let Some(duration) = source_duration.filter(|value| value.is_finite() && *value > 0.0) {
        let tolerance = source_tolerance.unwrap_or(0.5).clamp(0.25, 10.0);
        if last_end > duration + tolerance {
            bail!(
                "Harici altyazı zamanları medya süresinin dışında: {}",
                selected.language
            );
        }
    }
    Ok(())
}

pub(super) fn looks_like_srt(text: &str) -> bool {
    text.lines()
        .any(|line| line.contains("-->") && line.contains(','))
}

pub(super) fn direct_media_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    let path = url.path().rsplit('/').next().unwrap_or_default();
    let extension = path.rsplit('.').next().unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "3gp"
            | "aac"
            | "avi"
            | "flac"
            | "m4a"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp3"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "oga"
            | "ogg"
            | "opus"
            | "ts"
            | "wav"
            | "webm"
    )
}

pub(crate) fn validate_options(request: &AddRequest) -> Result<()> {
    if request
        .container
        .as_deref()
        .is_some_and(|c| !matches!(c, "mp4" | "mkv"))
    {
        bail!("Video kapsayıcısı MP4 veya MKV olmalıdır");
    }
    if request.max_height.is_some_and(|h| h == 0 || h > 16384) {
        bail!("Çözünürlük geçersiz");
    }
    if request.exact_height.is_some_and(|h| h == 0 || h > 16384) {
        bail!("Kesin çözünürlük geçersiz");
    }
    if request
        .subtitle_mode
        .as_deref()
        .is_some_and(|m| !matches!(m, "manual" | "automatic"))
    {
        bail!("Altyazı türü geçersiz");
    }
    if let Some(id) = &request.video_format_id {
        validate_format_id(id, "Görüntü format kimliği")?;
    }
    if let Some(id) = &request.audio_format_id {
        validate_format_id(id, "Ses format kimliği")?;
    }
    for selection in &request.audio_tracks {
        validate_audio_selection(selection)?;
    }
    if let Some(language) = &request.audio_language {
        validate_language(language, "Ses dili")?;
    }
    for selection in &request.subtitle_tracks {
        validate_subtitle_selection(selection)?;
    }
    let legacy_format = request
        .format_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty());
    if legacy_format
        && (request.exact_height.is_some()
            || request.video_format_id.is_some()
            || request.audio_format_id.is_some()
            || request.audio_language.is_some()
            || !request.audio_tracks.is_empty())
    {
        bail!("Format kimliği ile ayrı kalite/ses seçimi birlikte kullanılamaz");
    }
    if !request.audio_tracks.is_empty()
        && (request.audio_format_id.is_some() || request.audio_language.is_some())
    {
        bail!("Çoklu ses seçimi eski tek ses alanlarıyla birlikte kullanılamaz");
    }
    if !request.subtitle_tracks.is_empty()
        && (!request.subtitle_languages.is_empty() || request.subtitle_mode.is_some())
    {
        bail!("Açık altyazı seçimi eski altyazı alanlarıyla birlikte kullanılamaz");
    }
    if !request.external_subtitles.is_empty()
        && (!request.subtitle_tracks.is_empty()
            || !request.subtitle_languages.is_empty()
            || request.subtitle_mode.is_some())
    {
        bail!("Harici altyazılar çıkarıcı altyazı seçimleriyle birlikte kullanılamaz");
    }
    if request.subtitle_mode.is_some() && request.subtitle_languages.is_empty() {
        bail!("Altyazı türü seçildi ancak altyazı dili seçilmedi");
    }
    if let Some(items) = &request.playlist_items {
        if items.is_empty()
            || items.len() > 200
            || !items
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ',' | '-' | ':'))
        {
            bail!("Playlist öğeleri 1-10,15 biçiminde olmalı");
        }
    }
    if request.playlist
        && (legacy_format
            || request.video_format_id.is_some()
            || !request.audio_tracks.is_empty()
            || !request.external_subtitles.is_empty())
    {
        bail!("Playlist için öğeye özel biçim/ses/harici altyazı seçimi kullanılamaz");
    }
    if request.kind != DownloadKind::File {
        reject_unsafe_media_headers(&request.headers)?;
    }
    audio_conversion(request)?;
    validate_subtitle_languages(&request.subtitle_languages)?;
    validate_external_subtitles(&request.external_subtitles)?;
    let has_subtitles = !request.subtitle_languages.is_empty()
        || !request.subtitle_tracks.is_empty()
        || !request.external_subtitles.is_empty();
    if request.kind == DownloadKind::Audio && has_subtitles {
        bail!("Altyazı gömme yalnız video çıktısında desteklenir");
    }
    Ok(())
}
