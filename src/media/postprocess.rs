//! Post-processing of selected streams: muxing extra tracks and subtitle conversion.

use super::*;

/// Whether the selected audio or subtitle streams of the downloaded file need tagging or
/// merging. A converted audio file is a new encoding: the source track's codec and language
/// tag do not carry over to it, so there is nothing to tag or compare.
pub(super) fn stream_postprocess_needed(request: &AddRequest) -> Result<bool> {
    if audio_conversion(request)?.is_some() {
        return Ok(false);
    }
    Ok(!request.audio_tracks.is_empty()
        || request.audio_format_id.is_some()
        || request.audio_language.is_some()
        || !request.external_subtitles.is_empty())
}

pub(super) fn postprocess_selected_streams(
    path: &Path,
    request: &AddRequest,
    info: &MediaInfo,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<bool> {
    if !stream_postprocess_needed(request)? {
        return Ok(false);
    }
    let single_audio = request.audio_format_id.is_some() || request.audio_language.is_some();
    let audio_count = if request.audio_tracks.is_empty() {
        usize::from(single_audio)
    } else {
        request.audio_tracks.len()
    };
    let audio_language = |index: usize| selected_audio_language(request, index);
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
            bail!(crate::i18n::ui(
                "Seçilen ses akışları etiketlenmeden doğrulanamadı",
                "The selected audio streams could not be verified without tagging"
            ));
        }
        let metadata: Value = serde_json::from_slice(&probe.stdout)?;
        let audio = metadata["streams"]
            .as_array()
            .context(crate::i18n::ui(
                "Ses akış bilgisi yok",
                "There is no audio stream information",
            ))?
            .iter()
            .filter(|stream| stream["codec_type"] == "audio")
            .collect::<Vec<_>>();
        if audio.len() != audio_count {
            bail!(crate::i18n::ui(
                "Seçilen ses akış sayısı çıktı ile eşleşmiyor",
                "The number of selected audio streams does not match the output"
            ));
        }
        for (index, stream) in audio.iter().enumerate() {
            if let (Some(expected), Some(actual)) = (
                audio_language(index),
                stream["tags"]["language"]
                    .as_str()
                    .filter(|value| !matches!(*value, "und" | "unknown" | "")),
            ) {
                if language_key(expected) != language_key(actual) {
                    bail!(crate::i18n::ui("Ses çıktısının mevcut dil etiketi seçilen dille çelişiyor; etiket değiştirilmedi", "The existing language tag of the audio output conflicts with the selected language; the tag was not changed"));
                }
            }
        }
    }
    let parent = path.parent().context(crate::i18n::ui(
        "Medya çalışma klasörü eksik",
        "The media working folder is missing",
    ))?;
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
        .context(crate::i18n::ui(
            "Medya dosya türü eksik",
            "The media file type is missing",
        ))?;
    let staged = parent.join(format!("selected-output.{extension}"));
    if staged.exists() {
        fs::remove_file(&staged)?;
    }
    let args = selected_output_args(path, &subtitles, &staged, request, audio_count);
    let muxed = run_capture(
        &tools.ffmpeg,
        &args,
        b"",
        Duration::from_secs(10 * 60),
        control,
    )?;
    if !muxed.status.success() || !muxed.stderr.trim().is_empty() {
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!(
                    "Seçilen ses/altyazı akışları kayıpsız birleştirilemedi: {}",
                    bounded_tail(&muxed.stderr, 4096)
                ),
                format!(
                    "The selected audio/subtitle streams could not be merged losslessly: {}",
                    bounded_tail(&muxed.stderr, 4096)
                )
            )
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
        return Err(error).context(crate::i18n::ui(
            "Seçilen akış çıktısı çalışma dosyasına alınamadı",
            "Could not move the selected stream output into the working file",
        ));
    }
    fs::remove_file(backup)?;
    for subtitle in subtitles {
        fs::remove_file(subtitle)?;
    }
    Ok(true)
}

/// The language the selection expects for the audio stream at `index`.
fn selected_audio_language(request: &AddRequest, index: usize) -> Option<&str> {
    request
        .audio_tracks
        .get(index)
        .and_then(|selected| selected.language.as_deref())
        .or_else(|| {
            (request.audio_tracks.is_empty() && index == 0)
                .then_some(request.audio_language.as_deref())
                .flatten()
        })
}

/// FFmpeg arguments that copy the downloaded file and the external subtitles into `staged`,
/// tagging the selected audio and subtitle streams.
pub(super) fn selected_output_args(
    path: &Path,
    subtitles: &[PathBuf],
    staged: &Path,
    request: &AddRequest,
    audio_count: usize,
) -> Vec<String> {
    let mut args = vec![
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-nostdin".into(),
    ];
    // The whitelist is an input option: every input needs its own.
    for input in std::iter::once(path).chain(subtitles.iter().map(PathBuf::as_path)) {
        args.extend([
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-i".into(),
            input.to_string_lossy().into_owned(),
        ]);
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
    // A request without a container keeps the source's, so the subtitle codec and the language
    // tags follow the file being written.
    let extension = staged
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tag_container = matches!(extension.as_str(), "mp4" | "m4v" | "mov").then_some("mp4");
    if !subtitles.is_empty() {
        let codec = match extension.as_str() {
            _ if tag_container.is_some() => "mov_text",
            "webm" => "webvtt",
            _ => "srt",
        };
        args.extend(["-c:s".into(), codec.into()]);
    }
    for index in 0..audio_count {
        if let Some(language) = selected_audio_language(request, index) {
            args.extend([
                format!("-metadata:s:a:{index}"),
                format!("language={}", tag_language(language, tag_container)),
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
                tag_language(&selected.language, tag_container)
            ),
            format!("-metadata:s:s:{index}"),
            format!("title={}", selected.label),
            format!("-disposition:s:{index}"),
            if selected.is_default { "default" } else { "0" }.into(),
        ]);
    }
    args.push(staged.to_string_lossy().into_owned());
    args
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
    let target = Url::parse(&selected.url).context(crate::i18n::ui(
        "Harici altyazı adresi geçersiz",
        "The external subtitle URL is invalid",
    ))?;
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
    easy.useragent(tools::USER_AGENT)?;
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
            bail!(crate::i18n::ui(
                "Harici altyazı indirmesi durduruldu",
                "The external subtitle download was stopped"
            ));
        }
        if oversized {
            bail!(crate::i18n::ui(
                "Harici altyazı güvenlik boyutu sınırını aştı",
                "The external subtitle exceeded the safety size limit"
            ));
        }
        if write_failed {
            bail!(crate::i18n::ui(
                "Harici altyazı çalışma dosyasına yazılamadı",
                "Could not write the external subtitle to the working file"
            ));
        }
        return Err(error).context(crate::i18n::ui(
            "Harici altyazı indirilemedi",
            "Could not download the external subtitle",
        ));
    }
    file.flush()?;
    let status = easy.response_code()?;
    if (300..400).contains(&status) {
        fs::remove_file(path)?;
        bail!(crate::i18n::ui("Harici altyazı başka sunucuya yönlendirildi; hedefe özel başlıklar güvenle aktarılamadı", "The external subtitle was redirected to another server; target-specific headers could not be forwarded safely"));
    }
    if !(200..300).contains(&status) {
        fs::remove_file(path)?;
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!("Harici altyazı sunucusu HTTP {status} döndürdü"),
                format!("The external subtitle server returned HTTP {status}")
            )
        );
    }
    if received == 0 {
        fs::remove_file(path)?;
        bail!(crate::i18n::ui(
            "Harici altyazı boş döndü",
            "The external subtitle came back empty"
        ));
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
    let original = std::str::from_utf8(&bytes).context(crate::i18n::ui(
        "Harici altyazı UTF-8 değil",
        "The external subtitle is not UTF-8",
    ))?;
    let playable = without_unmuxable_cues(original);
    let text = playable.as_deref().unwrap_or(original);
    let is_vtt = text.trim_start_matches('\u{feff}').starts_with("WEBVTT");
    let mut cues = 0usize;
    let mut last_end = 0.0f64;
    for line in text.lines().filter(|line| line.contains("-->")) {
        let (start, end) = line.split_once("-->").context(crate::i18n::ui(
            "Harici altyazı zaman satırı geçersiz",
            "The external subtitle timing line is invalid",
        ))?;
        let start = parse_clock_timestamp(start.trim()).ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "Harici altyazı başlangıç zamanı geçersiz",
                "The external subtitle start time is invalid"
            ))
        })?;
        let end_token = end.split_ascii_whitespace().next().unwrap_or("");
        let end = parse_clock_timestamp(end_token).ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "Harici altyazı bitiş zamanı geçersiz",
                "The external subtitle end time is invalid"
            ))
        })?;
        if start < 0.0 || end <= start {
            bail!(crate::i18n::ui(
                "Harici altyazı zaman aralığı geçersiz",
                "The external subtitle time range is invalid"
            ));
        }
        cues += 1;
        last_end = last_end.max(end);
    }
    if cues == 0 || (!is_vtt && !looks_like_srt(text)) {
        bail!(crate::i18n::ui(
            "Harici track geçerli VTT veya SRT altyazısı değil",
            "The external track is not a valid VTT or SRT subtitle"
        ));
    }
    if let Some(duration) = source_duration.filter(|value| value.is_finite() && *value > 0.0) {
        let tolerance = source_tolerance.unwrap_or(0.5).clamp(0.25, 10.0);
        if last_end > duration + tolerance {
            bail!(
                "{}",
                crate::i18n::ui_owned!(
                    format!(
                        "Harici altyazı zamanları medya süresinin dışında: {}",
                        selected.language
                    ),
                    format!(
                        "The external subtitle timings fall outside the media duration: {}",
                        selected.language
                    )
                )
            );
        }
    }
    if let Some(playable) = playable {
        fs::write(path, playable)?;
    }
    Ok(())
}

/// Subtitle text without the cues FFmpeg cannot store, or `None` when every cue can stay.
/// A cue that does not end after it starts shows nothing and is left out. Cues that start at
/// the same moment become one cue: the subtitle demuxer hands them to the muxer with one
/// timestamp, which the MP4 muxer rejects ("Non-monotonic DTS"), and FFmpeg itself drops only
/// exact copies.
pub(super) fn without_unmuxable_cues(text: &str) -> Option<String> {
    let mut blocks = vec![Vec::new()];
    for line in text.lines() {
        if !line.trim().is_empty() {
            blocks.last_mut().unwrap().push(line);
        } else if !blocks.last().unwrap().is_empty() {
            blocks.push(Vec::new());
        }
    }
    let mut changed = false;
    let mut kept: Vec<Vec<std::borrow::Cow<str>>> = Vec::with_capacity(blocks.len());
    // Start time bits -> (kept block, its timing line, its end).
    let mut starts = std::collections::HashMap::new();
    for block in blocks.into_iter().filter(|block| !block.is_empty()) {
        let times = block
            .iter()
            .position(|line| line.contains("-->"))
            .and_then(|timing| {
                let (start, end, _) = timing_parts(block[timing])?;
                Some((
                    timing,
                    parse_clock_timestamp(start)?,
                    parse_clock_timestamp(end)?,
                ))
            });
        let Some((timing, start, end)) = times else {
            kept.push(block.into_iter().map(Into::into).collect());
            continue;
        };
        if end <= start {
            changed = true;
            continue;
        }
        let Some(&(index, kept_timing, kept_end)) = starts.get(&start.to_bits()) else {
            starts.insert(start.to_bits(), (kept.len(), timing, end));
            kept.push(block.into_iter().map(Into::into).collect());
            continue;
        };
        changed = true;
        let target = &mut kept[index];
        let lines = &block[timing + 1..];
        if !target[kept_timing + 1..]
            .windows(lines.len().max(1))
            .any(|kept_lines| kept_lines == lines)
        {
            target.extend(lines.iter().map(|&line| line.into()));
        }
        if end > kept_end {
            let (kept_start, _, settings) = timing_parts(&target[kept_timing]).unwrap();
            let (_, longer_end, _) = timing_parts(block[timing]).unwrap();
            target[kept_timing] = format!("{kept_start} --> {longer_end}{settings}").into();
            starts.insert(start.to_bits(), (index, kept_timing, end));
        }
    }
    changed.then(|| {
        let blocks = kept
            .iter()
            .map(|block| block.join("\n"))
            .collect::<Vec<_>>();
        blocks.join("\n\n") + "\n"
    })
}

/// The start, the end and the trailing cue settings of a `-->` timing line.
fn timing_parts(line: &str) -> Option<(&str, &str, &str)> {
    let (start, rest) = line.split_once("-->")?;
    let rest = rest.trim_start();
    let end = rest.split_ascii_whitespace().next()?;
    Some((start.trim(), end, &rest[end.len()..]))
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
        bail!(crate::i18n::ui(
            "Video kapsayıcısı MP4 veya MKV olmalıdır",
            "The video container must be MP4 or MKV"
        ));
    }
    if request.max_height.is_some_and(|h| h == 0 || h > 16384) {
        bail!(crate::i18n::ui(
            "Çözünürlük geçersiz",
            "The resolution is invalid"
        ));
    }
    if request.exact_height.is_some_and(|h| h == 0 || h > 16384) {
        bail!(crate::i18n::ui(
            "Kesin çözünürlük geçersiz",
            "The exact resolution is invalid"
        ));
    }
    if request
        .subtitle_mode
        .as_deref()
        .is_some_and(|m| !matches!(m, "manual" | "automatic"))
    {
        bail!(crate::i18n::ui(
            "Altyazı türü geçersiz",
            "The subtitle type is invalid"
        ));
    }
    if let Some(id) = &request.video_format_id {
        validate_format_id(
            id,
            crate::i18n::ui("Görüntü format kimliği", "Video format id"),
        )?;
    }
    if let Some(id) = &request.audio_format_id {
        validate_format_id(id, crate::i18n::ui("Ses format kimliği", "Audio format id"))?;
    }
    for selection in &request.audio_tracks {
        validate_audio_selection(selection)?;
    }
    if let Some(language) = &request.audio_language {
        validate_language(language, crate::i18n::ui("Ses dili", "Audio language"))?;
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
        bail!(crate::i18n::ui(
            "Format kimliği ile ayrı kalite/ses seçimi birlikte kullanılamaz",
            "A format id and a separate quality/audio selection cannot be used together"
        ));
    }
    if !request.audio_tracks.is_empty()
        && (request.audio_format_id.is_some() || request.audio_language.is_some())
    {
        bail!(crate::i18n::ui(
            "Çoklu ses seçimi eski tek ses alanlarıyla birlikte kullanılamaz",
            "Multiple audio selection cannot be used together with the legacy single audio fields"
        ));
    }
    if !request.subtitle_tracks.is_empty()
        && (!request.subtitle_languages.is_empty() || request.subtitle_mode.is_some())
    {
        bail!(crate::i18n::ui(
            "Açık altyazı seçimi eski altyazı alanlarıyla birlikte kullanılamaz",
            "Explicit subtitle selection cannot be used together with the legacy subtitle fields"
        ));
    }
    if !request.external_subtitles.is_empty()
        && (!request.subtitle_tracks.is_empty()
            || !request.subtitle_languages.is_empty()
            || request.subtitle_mode.is_some())
    {
        bail!(crate::i18n::ui(
            "Harici altyazılar çıkarıcı altyazı seçimleriyle birlikte kullanılamaz",
            "External subtitles cannot be used together with extractor subtitle selections"
        ));
    }
    if request.subtitle_mode.is_some() && request.subtitle_languages.is_empty() {
        bail!(crate::i18n::ui(
            "Altyazı türü seçildi ancak altyazı dili seçilmedi",
            "A subtitle type was selected but no subtitle language was selected"
        ));
    }
    if let Some(items) = &request.playlist_items {
        if items.is_empty()
            || items.len() > 200
            || !items
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ',' | '-' | ':'))
        {
            bail!(crate::i18n::ui(
                "Playlist öğeleri 1-10,15 biçiminde olmalı",
                "Playlist items must be in the form 1-10,15"
            ));
        }
    }
    if request.playlist
        && (legacy_format
            || request.video_format_id.is_some()
            || !request.audio_tracks.is_empty()
            || !request.external_subtitles.is_empty())
    {
        bail!(crate::i18n::ui(
            "Playlist için öğeye özel biçim/ses/harici altyazı seçimi kullanılamaz",
            "Per-item format/audio/external subtitle selection cannot be used for a playlist"
        ));
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
        bail!(crate::i18n::ui(
            "Altyazı gömme yalnız video çıktısında desteklenir",
            "Subtitle embedding is only supported for video output"
        ));
    }
    Ok(())
}
