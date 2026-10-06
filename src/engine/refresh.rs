//! Source-renewal identity: page and cookie context comparison, refreshed request contract and artifact archiving.

use super::*;

pub(super) fn normalized_page_identity(value: &str) -> Option<String> {
    let mut url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

pub(super) fn same_page_identity(left: &str, right: &str) -> bool {
    normalized_page_identity(left)
        .zip(normalized_page_identity(right))
        .is_some_and(|(left, right)| left == right)
}

pub(super) fn cookie_context(
    request: &AddRequest,
) -> Option<BTreeSet<(Option<String>, Option<String>)>> {
    if request
        .session_cookies
        .iter()
        .any(|cookie| cookie.store_id.is_none() && cookie.partition_key.is_none())
    {
        return None;
    }
    Some(
        request
            .session_cookies
            .iter()
            .map(|cookie| (cookie.store_id.clone(), cookie.partition_key.clone()))
            .collect(),
    )
}

pub(super) fn refresh_identity_matches(previous: &AddRequest, next: &AddRequest) -> bool {
    let page_matches = match (&previous.source_identity, &next.source_identity) {
        (Some(old), Some(new)) => {
            old.video_id == new.video_id
                && old.frame_id == new.frame_id
                && same_page_identity(&old.page_url, &new.page_url)
        }
        (None, None) => previous
            .page_url
            .as_deref()
            .and_then(normalized_page_identity)
            .is_some_and(|old| {
                next.page_url
                    .as_deref()
                    .and_then(normalized_page_identity)
                    .is_some_and(|new| old == new)
            }),
        _ => false,
    };
    let next_page_coherent = next.source_identity.as_ref().is_none_or(|identity| {
        next.page_url
            .as_deref()
            .and_then(normalized_page_identity)
            .is_none_or(|page| normalized_page_identity(&identity.page_url) == Some(page))
    });
    let external_identity_matches = previous.external_subtitles.len()
        == next.external_subtitles.len()
        && previous
            .external_subtitles
            .iter()
            .zip(&next.external_subtitles)
            .all(|(old, new)| {
                old.language == new.language
                    && old.label == new.label
                    && old.kind == new.kind
                    && old.is_default == new.is_default
            });
    page_matches
        && next_page_coherent
        && cookie_context(previous)
            .zip(cookie_context(next))
            .is_some_and(|(old, new)| old == new)
        && previous.kind == next.kind
        && previous.container == next.container
        && previous.max_height == next.max_height
        && previous.exact_height == next.exact_height
        && previous.format_id == next.format_id
        && previous.video_format_id == next.video_format_id
        && previous.audio_format == next.audio_format
        && previous.audio_format_id == next.audio_format_id
        && previous.audio_language == next.audio_language
        && previous.audio_tracks == next.audio_tracks
        && previous.subtitle_mode == next.subtitle_mode
        && previous.subtitle_languages == next.subtitle_languages
        && previous.subtitle_tracks == next.subtitle_tracks
        && external_identity_matches
        && previous.expected_audio == next.expected_audio
        && previous.playlist == next.playlist
}

pub(super) fn preserve_refresh_contract(previous: &AddRequest, next: &mut AddRequest) {
    next.kind = previous.kind;
    next.filename = previous.filename.clone();
    next.directory = previous.directory.clone();
    next.queue_id = previous.queue_id.clone();
    next.format_id = previous.format_id.clone();
    next.video_format_id = previous.video_format_id.clone();
    next.container = previous.container.clone();
    next.max_height = previous.max_height;
    next.exact_height = previous.exact_height;
    next.audio_format_id = previous.audio_format_id.clone();
    next.audio_language = previous.audio_language.clone();
    next.audio_tracks = previous.audio_tracks.clone();
    next.subtitle_mode = previous.subtitle_mode.clone();
    next.request_id = previous.request_id.clone();
    next.audio_format = previous.audio_format.clone();
    next.expected_audio = previous.expected_audio;
    next.subtitle_languages = previous.subtitle_languages.clone();
    next.subtitle_tracks = previous.subtitle_tracks.clone();
    next.playlist = previous.playlist;
    next.connections = previous.connections;
    next.checksum = previous.checksum.clone();
    next.start_at = previous.start_at;
    next.full_verification = previous.full_verification;
    next.page_url = previous.page_url.clone();
    if let (Some(old), Some(new)) = (&previous.source_identity, &mut next.source_identity) {
        new.video_id = old.video_id.clone();
        new.frame_id = old.frame_id;
        new.page_url = old.page_url.clone();
    }
}

pub(super) fn refresh_output_path(job: &Job) -> Result<PathBuf> {
    match &job.work_dir {
        Some(directory) => Ok(directory.join(
            job.path
                .file_name()
                .context("Kaynak yenileme çıktı adı eksik")?,
        )),
        None => Ok(job.path.clone()),
    }
}

pub(super) fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

pub(super) struct RefreshArchive {
    pub(super) moves: Vec<(PathBuf, PathBuf)>,
}

pub(super) fn restore_refresh_archive(archive: &RefreshArchive) -> Result<()> {
    for (original, saved) in archive.moves.iter().rev() {
        if original.exists() {
            bail!(
                "{}: {}",
                crate::i18n::ui(
                    "Arşivlenen çalışma verisi geri alınırken hedef yeniden oluştu",
                    "The target reappeared while restoring the archived work data"
                ),
                original.display()
            );
        }
        fs::rename(saved, original).with_context(|| {
            format!(
                "Arşivlenen çalışma verisi geri alınamadı: {}",
                saved.display()
            )
        })?;
    }
    Ok(())
}

pub(super) fn archive_refresh_artifacts(
    job: &Job,
    output: &Path,
) -> Result<Option<RefreshArchive>> {
    let archive_id = format!(
        "{}-{}",
        Utc::now().timestamp_millis(),
        Uuid::new_v4().simple()
    );
    if let Some(work) = &job.work_dir {
        if !work.exists() {
            return Ok(None);
        }
        if fs::read_to_string(work.join(".job")).ok().as_deref() != Some(&job.id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kaynak yenileme çalışma klasörü bu işe ait değil; eserler korundu",
                    "The source-refresh work folder does not belong to this job; artifacts were kept"
                )
            );
        }
        let archive_root = work
            .parent()
            .and_then(Path::parent)
            .context("Kaynak yenileme arşiv klasörü bulunamadı")?
            .join("refresh-archives")
            .join(&job.id);
        fs::create_dir_all(&archive_root)?;
        let saved = archive_root.join(archive_id);
        fs::rename(work, &saved).context("Önceki çalışma verisi arşivlenemedi")?;
        return Ok(Some(RefreshArchive {
            moves: vec![(work.clone(), saved)],
        }));
    }

    let mut candidates = vec![
        append_path_suffix(output, ".ssdownload.part"),
        append_path_suffix(output, ".ssdownload.state"),
    ];
    candidates.extend(
        (0..16).map(|index| append_path_suffix(output, &format!(".ssdownload.part.{index:03}"))),
    );
    if let Ok(owner) = crate::output::identity(output) {
        let workspace = output
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".ssdownload-work")
            .join(&owner);
        if workspace.exists() {
            if fs::read_to_string(workspace.join(".owner")).ok().as_deref() != Some(&owner) {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Medya çalışma alanının sahipliği doğrulanamadı; eserler korundu",
                        "Media workspace ownership could not be verified; artifacts were kept"
                    )
                );
            }
            candidates.push(workspace);
        }
    }
    candidates.retain(|path| path.exists());
    if candidates.is_empty() {
        return Ok(None);
    }
    let archive_root = output
        .parent()
        .context("Kaynak yenileme çıktı klasörü eksik")?
        .join(".ssdownload-work")
        .join("refresh-archives")
        .join(&job.id)
        .join(archive_id);
    fs::create_dir_all(&archive_root)?;
    let mut archive = RefreshArchive { moves: Vec::new() };
    for (index, original) in candidates.into_iter().enumerate() {
        let saved = archive_root.join(format!("artifact-{index:03}"));
        if let Err(error) = fs::rename(&original, &saved) {
            let _ = restore_refresh_archive(&archive);
            return Err(error).context("Önceki çalışma eseri arşivlenemedi");
        }
        archive.moves.push((original, saved));
    }
    Ok(Some(archive))
}
