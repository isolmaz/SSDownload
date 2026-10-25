//! Owned media workspaces, output plans and source-refresh staging.

use super::*;

/// Extensions a chosen name may already carry for the container it will be saved in. Any other
/// dot belongs to the title ("Python 3.12 guide", "Mr. Robot") and must stay in the file name.
const MEDIA_NAME_EXTENSIONS: &[&str] = &[
    "3gp", "aac", "avi", "flac", "m3u8", "m4a", "m4v", "mka", "mkv", "mov", "mp3", "mp4", "mpd",
    "mpeg", "mpg", "oga", "ogg", "opus", "ts", "wav", "webm",
];

/// Per-download copies of the extractor document (signed URLs, header values). They live in the
/// profile's private cookie-jar folder so the start-up sweep removes what a hard kill left.
pub(super) const INFO_LEASE_PREFIX: &str = "ssdownload-source-info-";
pub(super) const PINNED_LEASE_PREFIX: &str = "ssdownload-pinned-source-";

pub(super) fn without_media_extension(output: &Path) -> PathBuf {
    let known = output
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            MEDIA_NAME_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        });
    if known {
        output.with_extension("")
    } else {
        output.to_path_buf()
    }
}

pub(super) fn with_media_extension(output: &Path, extension: &std::ffi::OsStr) -> PathBuf {
    let mut name = without_media_extension(output).into_os_string();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

pub(super) struct OutputPlan {
    pub(super) template: PathBuf,
    pub(super) workspace: PathBuf,
    pub(super) owner: String,
}
impl OutputPlan {
    pub(super) fn new(
        output: &Path,
        request: &AddRequest,
        evidence: Option<&MediaSourceEvidence>,
    ) -> Result<Self> {
        use std::os::windows::fs::MetadataExt;
        let playlist = request.playlist;
        let fingerprint = request_fingerprint(request)?;
        let owner = crate::output::identity(output)?;
        let workspace = workspace_path(output, &owner);
        let parent = workspace.parent().context(crate::i18n::ui(
            "Çalışma klasörü eksik",
            "The working folder is missing",
        ))?;
        if parent.exists() && fs::symlink_metadata(parent)?.file_attributes() & 0x400 != 0 {
            bail!(crate::i18n::ui(
                "Çalışma klasörü yönlendirme içeriyor",
                "The working folder contains a redirect"
            ));
        }
        if workspace.exists() {
            verify_workspace(&workspace, &owner)?;
            reconcile_media_workspace(&workspace, &fingerprint, evidence)?;
        } else {
            fs::create_dir_all(workspace.parent().context(crate::i18n::ui(
                "Çalışma klasörü eksik",
                "The working folder is missing",
            ))?)?;
            fs::create_dir(&workspace)?;
            fs::write(workspace.join(".owner"), &owner)?;
            if let Some(evidence) = evidence {
                write_media_source_state(
                    &workspace,
                    &MediaSourceState {
                        version: MEDIA_SOURCE_STATE_VERSION,
                        request: fingerprint.clone(),
                        evidence: evidence.clone(),
                    },
                )?;
            }
            crate::recovery::atomic_write(
                workspace.join(".request").as_path(),
                fingerprint.as_bytes(),
            )?;
        }
        // canonicalize supplies the Windows extended-length prefix. The nested
        // private job directory can exceed MAX_PATH even for ordinary titles;
        // external Python/FFmpeg processes need that prefix to create files.
        let literal = workspace
            .canonicalize()?
            .to_string_lossy()
            .replace('%', "%%");
        let pattern = if playlist {
            "%(playlist_index)03d - %(title).120s.%(ext)s"
        } else {
            "media.%(ext)s"
        };
        Ok(Self {
            template: PathBuf::from(format!("{literal}\\{pattern}")),
            workspace,
            owner,
        })
    }

    pub(super) fn checked_output(&self, path: &Path) -> Result<PathBuf> {
        verify_workspace(&self.workspace, &self.owner)?;
        let root = self.workspace.canonicalize()?;
        let resolved = path.canonicalize()?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            bail!(crate::i18n::ui(
                "Medya aracı beklenen klasör dışında bir çıktı döndürdü",
                "The media tool returned an output outside the expected folder"
            ));
        }
        Ok(resolved)
    }

    pub(super) fn publish(
        &self,
        output: &Path,
        playlist: bool,
        completed: &HashSet<PathBuf>,
        last: Option<&Path>,
    ) -> Result<PathBuf> {
        verify_workspace(&self.workspace, &self.owner)?;
        let final_path = if playlist {
            if completed.is_empty() {
                bail!(crate::i18n::ui(
                    "Tamamlanan playlist dosyası bulunamadı",
                    "The completed playlist file was not found"
                ));
            }
            let ready = self.workspace.join("ready");
            fs::create_dir_all(&ready)?;
            for path in completed {
                let path = self.checked_output(path)?;
                let target = ready.join(path.file_name().context(crate::i18n::ui(
                    "Dosya adı eksik",
                    "The file name is missing",
                ))?);
                if !target.exists() {
                    fs::rename(&path, &target)?;
                }
            }
            crate::output::publish(&ready, &without_media_extension(output))?
        } else {
            let path = self.checked_output(last.context(crate::i18n::ui(
                "Tamamlanan medya dosyası bulunamadı",
                "The completed media file was not found",
            ))?)?;
            let target = with_media_extension(
                output,
                path.extension().context(crate::i18n::ui(
                    "Medya dosya türü eksik",
                    "The media file type is missing",
                ))?,
            );
            crate::output::publish(&path, &target)?
        };
        // This is a private, ownership-checked tree. Partial files remain here on pause/failure.
        // A cleanup failure must not turn an already published video into a duplicate retry.
        let _ = remove_workspace(&self.workspace, &self.owner);
        Ok(final_path)
    }

    pub(super) fn install_pinned_source(&self, source: &PinnedMediaSource) -> Result<()> {
        verify_workspace(&self.workspace, &self.owner)?;
        let serialized = serde_json::to_string(source)?;
        let sealed = crate::secure::seal(serialized)?;
        crate::recovery::atomic_write(&self.workspace.join(MEDIA_PINNED_FILE), sealed.as_bytes())
    }
}

pub(super) struct SourceInfoLease {
    pub(super) root: PathBuf,
    pub(super) info_path: PathBuf,
}

impl SourceInfoLease {
    pub(super) fn from_pinned_workspace(workspace: &Path) -> Result<Self> {
        let sealed =
            fs::read_to_string(workspace.join(MEDIA_PINNED_FILE)).context(crate::i18n::ui(
                "Doğrulanmış medya kaynak paketi okunamadı",
                "Could not read the verified media source bundle",
            ))?;
        let serialized = crate::secure::unseal(&sealed).context(crate::i18n::ui(
            "Doğrulanmış medya kaynak paketi açılamadı",
            "Could not open the verified media source bundle",
        ))?;
        let mut source: PinnedMediaSource =
            serde_json::from_str(&serialized).context(crate::i18n::ui(
                "Doğrulanmış medya kaynak paketi bozuk",
                "The verified media source bundle is corrupt",
            ))?;
        reject_file_urls(&source.inspection)?;
        let root = crate::paths::cookie_jar_dir()?
            .join(format!("{PINNED_LEASE_PREFIX}{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<PathBuf> {
            for (index, manifest) in source.manifests.iter().enumerate() {
                let path = root.join(format!("manifest-{index}.m3u8"));
                fs::write(&path, manifest.body.as_bytes())?;
                let url = Url::from_file_path(&path).map_err(|_| {
                    anyhow!(crate::i18n::ui(
                        "Doğrulanmış manifest dosya adresine dönüştürülemedi",
                        "The verified manifest could not be converted to a file URL"
                    ))
                })?;
                replace_marker(&mut source.inspection, &manifest.marker, url.as_str());
            }
            if value_contains_marker(&source.inspection) {
                bail!(crate::i18n::ui(
                    "Doğrulanmış medya kaynak paketi eksik manifest içeriyor",
                    "The verified media source bundle has a missing manifest"
                ));
            }
            let info_path = root.join("source.info.json");
            fs::write(&info_path, serde_json::to_vec(&source.inspection)?)?;
            Ok(info_path)
        })();
        match result {
            Ok(info_path) => Ok(Self { root, info_path }),
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                Err(error)
            }
        }
    }

    pub(super) fn from_inspection(inspection: &Value) -> Result<Self> {
        reject_file_urls(inspection)?;
        let serialized = serde_json::to_vec(inspection)?;
        let root = crate::paths::cookie_jar_dir()?
            .join(format!("{INFO_LEASE_PREFIX}{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        let info_path = root.join("source.info.json");
        if let Err(error) = fs::write(&info_path, serialized) {
            let _ = fs::remove_dir_all(&root);
            return Err(error.into());
        }
        Ok(Self { root, info_path })
    }
}

impl Drop for SourceInfoLease {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The extractor document is built from remote site data and yt-dlp downloads from it: none of
/// its addresses may name a local file or a network share (`file:////host/share/…` makes
/// Windows authenticate to the host).
pub(super) fn reject_file_urls(document: &Value) -> Result<()> {
    match document {
        Value::Object(fields) => {
            for (key, field) in fields {
                if matches!(
                    key.as_str(),
                    "url" | "manifest_url" | "fragment_base_url" | "path"
                ) && field
                    .as_str()
                    .and_then(|value| Url::parse(value).ok())
                    .is_some_and(|url| url.scheme() == "file")
                {
                    bail!(crate::i18n::ui(
                        "Medya kaynağı yerel dosya veya ağ paylaşımı adresi içeriyor; güvenlik için indirilmedi",
                        "The media source lists a local file or network share address; it was not downloaded for safety"
                    ));
                }
                reject_file_urls(field)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_file_urls(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn replace_marker(value: &mut Value, marker: &str, replacement: &str) {
    match value {
        Value::String(text) if text == marker => *text = replacement.into(),
        Value::Array(values) => {
            for value in values {
                replace_marker(value, marker, replacement);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_marker(value, marker, replacement);
            }
        }
        _ => {}
    }
}

pub(super) fn value_contains_marker(value: &Value) -> bool {
    match value {
        Value::String(text) => text.starts_with(PINNED_URL_PREFIX),
        Value::Array(values) => values.iter().any(value_contains_marker),
        Value::Object(values) => values.values().any(value_contains_marker),
        _ => false,
    }
}

pub(super) fn promote_pinned_source(workspace: &Path) -> Result<()> {
    let pending = workspace.join(MEDIA_PINNED_PENDING_FILE);
    let sealed = fs::read(&pending).context(crate::i18n::ui(
        "Yenilenen doğrulanmış medya kaynak paketi bulunamadı",
        "The refreshed verified media source bundle was not found",
    ))?;
    // Validate that the pending payload is both DPAPI-protected and structurally
    // complete before making it current. The plaintext exists only in memory.
    let opened = crate::secure::unseal(std::str::from_utf8(&sealed).context(crate::i18n::ui(
        "Doğrulanmış medya kaynak paketi metin değil",
        "The verified media source bundle is not text",
    ))?)?;
    let source: PinnedMediaSource = serde_json::from_str(&opened).context(crate::i18n::ui(
        "Yenilenen doğrulanmış medya kaynak paketi bozuk",
        "The refreshed verified media source bundle is corrupt",
    ))?;
    validate_pinned_source(&source)?;
    crate::recovery::atomic_write(&workspace.join(MEDIA_PINNED_FILE), &sealed)?;
    fs::remove_file(pending)?;
    Ok(())
}

pub(super) fn reconcile_media_workspace(
    workspace: &Path,
    fingerprint: &str,
    evidence: Option<&MediaSourceEvidence>,
) -> Result<()> {
    let source_path = workspace.join(MEDIA_SOURCE_FILE);
    match evidence {
        Some(evidence) => {
            let current = read_media_source_state(workspace).map_err(|error| {
                anyhow!(
                    "{}",
                    crate::i18n::ui_owned!(
                        format!("Mevcut HLS/DASH parçalarının uyumluluk kanıtı yok veya bozuk; eserler korundu. Açık yeniden başlatma gerekir: {error:#}"),
                        format!("The compatibility evidence for the existing HLS/DASH segments is missing or corrupt; work files were kept. An explicit restart is required: {error:#}")
                    )
                )
            })?;
            if current.request == fingerprint && current.evidence == *evidence {
                crate::recovery::atomic_write(
                    workspace.join(".request").as_path(),
                    fingerprint.as_bytes(),
                )?;
                let _ = fs::remove_file(workspace.join(MEDIA_SOURCE_PENDING_FILE));
                let _ = fs::remove_file(workspace.join(MEDIA_PINNED_PENDING_FILE));
                return Ok(());
            }
            let pending: PendingMediaSourceState = serde_json::from_slice(
                &fs::read(workspace.join(MEDIA_SOURCE_PENDING_FILE)).context(
                    crate::i18n::ui("Yenilenen HLS/DASH kaynağının atomik geçiş kaydı bulunamadı; eserler korundu", "The atomic handover record of the refreshed HLS/DASH source was not found; work files were kept"),
                )?,
            )
            .context(crate::i18n::ui("Yenilenen HLS/DASH kaynağının atomik geçiş kaydı bozuk", "The atomic handover record of the refreshed HLS/DASH source is corrupt"))?;
            if pending.version != MEDIA_SOURCE_STATE_VERSION
                || pending.previous_request != current.request
                || pending.next.request != fingerprint
                || pending.next.evidence != *evidence
                || pending.next.evidence.live
            {
                bail!(crate::i18n::ui("Yenilenen HLS/DASH kaynak/akış/parça çizelgesi mevcut çalışmayla eşleşmiyor; eserler korundu. Açık yeniden başlatma gerekir", "The refreshed HLS/DASH source/stream/segment timeline does not match the existing work; work files were kept. An explicit restart is required"));
            }
            promote_pinned_source(workspace)?;
            write_media_source_state(workspace, &pending.next)?;
            crate::recovery::atomic_write(
                workspace.join(".request").as_path(),
                fingerprint.as_bytes(),
            )?;
            fs::remove_file(workspace.join(MEDIA_SOURCE_PENDING_FILE))?;
            Ok(())
        }
        None => {
            if source_path.exists() {
                bail!(crate::i18n::ui("Mevcut çalışma HLS/DASH parça kanıtı içeriyor ancak yeni kaynak tam parça çizelgesi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir", "The existing work has HLS/DASH segment evidence but the new source offers no complete segment timeline; work files were kept. An explicit restart is required"));
            }
            if fs::read_to_string(workspace.join(".request"))? != fingerprint {
                bail!(crate::i18n::ui("Bu hedefin geçici dosyaları başka bir indirmeye ait. Yeni bir dosya adı seçin.", "The temporary files for this target belong to another download. Choose a new file name."));
            }
            Ok(())
        }
    }
}

pub(super) fn workspace_path(output: &Path, owner: &str) -> PathBuf {
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".ssdownload-work")
        .join(owner)
}
pub(super) fn verify_workspace(path: &Path, owner: &str) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    if fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0
        || fs::read_to_string(path.join(".owner"))? != owner
    {
        bail!(crate::i18n::ui(
            "Çalışma klasörünün SSDownload'a ait olduğu doğrulanamadı",
            "Could not verify that the working folder belongs to SSDownload"
        ));
    }
    let parent = path.parent().context(crate::i18n::ui(
        "Çalışma klasörü eksik",
        "The working folder is missing",
    ))?;
    if fs::symlink_metadata(parent)?.file_attributes() & 0x400 != 0 {
        bail!(crate::i18n::ui(
            "Çalışma klasörü yönlendirme içeriyor",
            "The working folder contains a redirect"
        ));
    }
    Ok(())
}
/// Deletes the files this attempt produced inside the workspace, keeping everything that was
/// already finished before it started. A skipped fragment invalidates only the streams of the
/// attempt that saw it, so earlier, complete streams are not downloaded again.
pub(super) fn discard_attempt_outputs(
    plan: &OutputPlan,
    keep: &HashSet<PathBuf>,
) -> Result<(usize, u64)> {
    use std::os::windows::fs::MetadataExt;
    verify_workspace(&plan.workspace, &plan.owner)?;
    let stem = plan
        .template
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("media")
        .split('%')
        .next()
        .unwrap_or("media")
        .to_owned();
    // The tool reports files under the extended-length form of the workspace, which does not
    // compare equal to the plain workspace path, so kept files are matched by resolved identity.
    let keep = keep
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect::<HashSet<_>>();
    let mut removed = 0usize;
    let mut bytes = 0u64;
    for entry in fs::read_dir(&plan.workspace)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_attributes() & 0x400 != 0 {
            bail!(crate::i18n::ui(
                "Çalışma klasörü yönlendirme içeriyor",
                "The working folder contains a redirect"
            ));
        }
        if !meta.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };
        // `.owner`, `.request`, `.source*` are workspace state, never tool output: the playlist
        // template has no literal prefix, so the stem alone would match them too.
        if name.starts_with('.')
            || !name.starts_with(&stem)
            || path
                .canonicalize()
                .is_ok_and(|resolved| keep.contains(&resolved))
        {
            continue;
        }
        let size = meta.len();
        fs::remove_file(&path)?;
        removed += 1;
        bytes = bytes.saturating_add(size);
    }
    Ok((removed, bytes))
}

pub(super) fn remove_workspace(path: &Path, owner: &str) -> Result<()> {
    verify_workspace(path, owner)?;
    // Refuse junctions, including nested ones, before any recursive removal.
    fn check_tree(path: &Path) -> Result<()> {
        use std::os::windows::fs::MetadataExt;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.file_attributes() & 0x400 != 0 {
                bail!(crate::i18n::ui(
                    "Çalışma klasörü yönlendirme içeriyor",
                    "The working folder contains a redirect"
                ));
            }
            if meta.is_dir() {
                check_tree(&entry.path())?;
            }
        }
        Ok(())
    }
    check_tree(path)?;
    fs::remove_dir_all(path)?;
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}
pub(crate) fn has_partial_output(output: &Path) -> bool {
    crate::output::identity(output)
        .map(|owner| workspace_path(output, &owner).exists())
        .unwrap_or(false)
}

#[derive(Debug)]
pub(crate) struct PreparedSourceRefresh {
    action: PreparedSourceRefreshAction,
}

#[derive(Debug)]
pub(super) enum PreparedSourceRefreshAction {
    Noop,
    Reuse {
        workspace: PathBuf,
        owner: String,
        pending_state: Vec<u8>,
        pending_pinned: Vec<u8>,
    },
}

pub(crate) fn prepare_source_refresh(
    paths: &AppPaths,
    previous: &AddRequest,
    next: &AddRequest,
    output: &Path,
    restart: bool,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<PreparedSourceRefresh> {
    if control.stop_requested() {
        bail!(crate::i18n::ui(
            "Kaynak hazırlığı durduruldu",
            "Source preparation was stopped"
        ));
    }
    if selection_fingerprint(previous)? != selection_fingerprint(next)? {
        bail!(crate::i18n::ui(
            "Kaynak yenilemesi seçilen görüntü/ses/altyazı kimliğini değiştiremez",
            "A source refresh cannot change the selected video/audio/subtitle identity"
        ));
    }
    // The engine's job-level archive owns explicit restart and its rollback.
    // Media preparation stays pure and does not move retained work.
    if restart {
        return Ok(PreparedSourceRefresh {
            action: PreparedSourceRefreshAction::Noop,
        });
    }
    // A job creates its work folder only when it first transfers, and the
    // output identity is resolved by canonicalizing that folder. A job that
    // never transferred has neither, and that is "nothing to reuse", not a
    // failed refresh: the engine rebinds the source from scratch.
    if !output.parent().is_some_and(|parent| parent.exists()) {
        return Ok(PreparedSourceRefresh {
            action: PreparedSourceRefreshAction::Noop,
        });
    }
    let owner = crate::output::identity(output)?;
    let workspace = workspace_path(output, &owner);
    if !workspace.exists() {
        return Ok(PreparedSourceRefresh {
            action: PreparedSourceRefreshAction::Noop,
        });
    }
    verify_workspace(&workspace, &owner)?;
    let current = read_media_source_state(&workspace).map_err(|error| {
        anyhow!(
            "{}",
            crate::i18n::ui_owned!(
                format!("Mevcut HLS/DASH parçalarının kalıcı kaynak/akış/parça kanıtı yok; eserler korundu. Açık yeniden başlatma gerekir: {error:#}"),
                format!("The persistent source/stream/segment evidence for the existing HLS/DASH segments is missing; work files were kept. An explicit restart is required: {error:#}")
            )
        )
    })?;
    let previous_fingerprint = request_fingerprint(previous)?;
    if current.request != previous_fingerprint
        || current.evidence.selection != selection_fingerprint(previous)?
    {
        bail!(crate::i18n::ui("Mevcut HLS/DASH çalışma alanı önceki iş ve seçim kimliğiyle eşleşmiyor; eserler korundu", "The existing HLS/DASH workspace does not match the previous job and selection identity; work files were kept"));
    }
    if current.evidence.live {
        bail!(crate::i18n::ui("Canlı HLS/DASH çizelgesi zamanla değişebildiği için mevcut parçalar yenilenen kaynakla birleştirilemez; eserler korundu. Açık yeniden başlatma gerekir", "A live HLS/DASH timeline can change over time, so the existing segments cannot be combined with the refreshed source; work files were kept. An explicit restart is required"));
    }

    let inspect_request = InspectRequest {
        url: next.url.clone(),
        headers: next.headers.clone(),
        referer: next.referer.clone(),
        page_url: next.page_url.clone(),
        session_cookies: next.session_cookies.clone(),
        playlist: next.playlist,
        ..InspectRequest::default()
    };
    let toolchain = tools::verified_toolchain(paths, control)?;
    let (inspection, info) =
        inspect_document_with_tools(&inspect_request, &toolchain, control, network)?;
    if control.stop_requested() {
        bail!(crate::i18n::ui(
            "Kaynak hazırlığı durduruldu",
            "Source preparation was stopped"
        ));
    }
    validate_track_selection(next, &info)?;
    validate_container_selection(next, &info)?;
    let selector = format_selector(next, &info)?;
    let capture = capture_media_source(next, &inspection, &info, &selector, control, network)?;
    let next_evidence = capture.evidence.context(
        crate::i18n::ui("Yenilenen kaynak desteklenen, tam ve bütçe içinde doğrulanmış bir sonlu HLS/DASH parça çizelgesi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir", "The refreshed source does not offer a supported, complete, budget-verified finite HLS/DASH segment timeline; work files were kept. An explicit restart is required"),
    )?;
    let pinned = capture.pinned.context(
        crate::i18n::ui("Yenilenen kaynak indiriciye sabitlenebilen HLS/DASH parça bilgisi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir", "The refreshed source does not offer HLS/DASH segment information that can be pinned for the downloader; work files were kept. An explicit restart is required"),
    )?;
    if current.evidence != next_evidence {
        bail!(crate::i18n::ui("Yenilenen HLS/DASH kaynağı seçilen akışların parça kimlik veya zaman çizelgesini değiştirdi; eserler korundu. Açık yeniden başlatma gerekir", "The refreshed HLS/DASH source changed the segment identity or timeline of the selected streams; work files were kept. An explicit restart is required"));
    }
    if control.stop_requested() {
        bail!(crate::i18n::ui(
            "Kaynak hazırlığı durduruldu",
            "Source preparation was stopped"
        ));
    }
    let pending = PendingMediaSourceState {
        version: MEDIA_SOURCE_STATE_VERSION,
        previous_request: previous_fingerprint,
        next: MediaSourceState {
            version: MEDIA_SOURCE_STATE_VERSION,
            request: request_fingerprint(next)?,
            evidence: next_evidence,
        },
    };
    let pending_state = serde_json::to_vec(&pending)?;
    let pending_pinned = crate::secure::seal(serde_json::to_string(&pinned)?)?.into_bytes();
    Ok(PreparedSourceRefresh {
        action: PreparedSourceRefreshAction::Reuse {
            workspace,
            owner,
            pending_state,
            pending_pinned,
        },
    })
}

pub(crate) fn stage_source_refresh(prepared: &PreparedSourceRefresh) -> Result<bool> {
    let PreparedSourceRefreshAction::Reuse {
        workspace,
        owner,
        pending_state,
        pending_pinned,
    } = &prepared.action
    else {
        return Ok(false);
    };
    verify_workspace(workspace, owner)?;
    crate::recovery::atomic_write(&workspace.join(MEDIA_PINNED_PENDING_FILE), pending_pinned)?;
    if let Err(error) =
        crate::recovery::atomic_write(&workspace.join(MEDIA_SOURCE_PENDING_FILE), pending_state)
    {
        let _ = fs::remove_file(workspace.join(MEDIA_PINNED_PENDING_FILE));
        return Err(error);
    }
    Ok(true)
}

pub(crate) fn finish_source_refresh(output: &Path) -> Result<()> {
    let owner = crate::output::identity(output)?;
    let workspace = workspace_path(output, &owner);
    if !workspace.exists() || !workspace.join(MEDIA_SOURCE_PENDING_FILE).exists() {
        return Ok(());
    }
    verify_workspace(&workspace, &owner)?;
    let current = read_media_source_state(&workspace)?;
    let pending: PendingMediaSourceState =
        serde_json::from_slice(&fs::read(workspace.join(MEDIA_SOURCE_PENDING_FILE))?)?;
    if pending.version != MEDIA_SOURCE_STATE_VERSION
        || pending.previous_request != current.request
        || pending.next.evidence.live
    {
        bail!(crate::i18n::ui(
            "HLS/DASH kaynak yenileme geçiş kaydı mevcut çalışma alanıyla eşleşmiyor",
            "The HLS/DASH source refresh handover record does not match the existing workspace"
        ))
    }
    promote_pinned_source(&workspace)?;
    write_media_source_state(&workspace, &pending.next)?;
    crate::recovery::atomic_write(
        workspace.join(".request").as_path(),
        pending.next.request.as_bytes(),
    )?;
    fs::remove_file(workspace.join(MEDIA_SOURCE_PENDING_FILE))?;
    Ok(())
}

pub(crate) fn cancel_source_refresh(output: &Path) -> Result<()> {
    let owner = crate::output::identity(output)?;
    let workspace = workspace_path(output, &owner);
    if workspace.exists() {
        verify_workspace(&workspace, &owner)?;
        for name in [MEDIA_SOURCE_PENDING_FILE, MEDIA_PINNED_PENDING_FILE] {
            let pending = workspace.join(name);
            if pending.exists() {
                fs::remove_file(pending)?;
            }
        }
    }
    Ok(())
}
