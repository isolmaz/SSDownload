//! Owned media workspaces, output plans and source-refresh staging.

use super::*;

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
        let parent = workspace.parent().context("Çalışma klasörü eksik")?;
        if parent.exists() && fs::symlink_metadata(parent)?.file_attributes() & 0x400 != 0 {
            bail!("Çalışma klasörü yönlendirme içeriyor");
        }
        if workspace.exists() {
            verify_workspace(&workspace, &owner)?;
            reconcile_media_workspace(&workspace, &fingerprint, evidence)?;
        } else {
            fs::create_dir_all(workspace.parent().context("Çalışma klasörü eksik")?)?;
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
            bail!("Medya aracı beklenen klasör dışında bir çıktı döndürdü");
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
                bail!("Tamamlanan playlist dosyası bulunamadı");
            }
            let ready = self.workspace.join("ready");
            fs::create_dir_all(&ready)?;
            for path in completed {
                let path = self.checked_output(path)?;
                let target = ready.join(path.file_name().context("Dosya adı eksik")?);
                if !target.exists() {
                    fs::rename(&path, &target)?;
                }
            }
            crate::output::publish(&ready, &output.with_extension(""))?
        } else {
            let path = self.checked_output(last.context("Tamamlanan medya dosyası bulunamadı")?)?;
            let target = output.with_extension(path.extension().context("Medya dosya türü eksik")?);
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
        let sealed = fs::read_to_string(workspace.join(MEDIA_PINNED_FILE))
            .context("Doğrulanmış medya kaynak paketi okunamadı")?;
        let serialized =
            crate::secure::unseal(&sealed).context("Doğrulanmış medya kaynak paketi açılamadı")?;
        let mut source: PinnedMediaSource =
            serde_json::from_str(&serialized).context("Doğrulanmış medya kaynak paketi bozuk")?;
        let root =
            std::env::temp_dir().join(format!("ssdownload-pinned-source-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<PathBuf> {
            for (index, manifest) in source.manifests.iter().enumerate() {
                let path = root.join(format!("manifest-{index}.m3u8"));
                fs::write(&path, manifest.body.as_bytes())?;
                let url = Url::from_file_path(&path)
                    .map_err(|_| anyhow!("Doğrulanmış manifest dosya adresine dönüştürülemedi"))?;
                replace_marker(&mut source.inspection, &manifest.marker, url.as_str());
            }
            if value_contains_marker(&source.inspection) {
                bail!("Doğrulanmış medya kaynak paketi eksik manifest içeriyor");
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
        let serialized = serde_json::to_vec(inspection)?;
        let root =
            std::env::temp_dir().join(format!("ssdownload-source-info-{}", uuid::Uuid::new_v4()));
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
    let sealed =
        fs::read(&pending).context("Yenilenen doğrulanmış medya kaynak paketi bulunamadı")?;
    // Validate that the pending payload is both DPAPI-protected and structurally
    // complete before making it current. The plaintext exists only in memory.
    let opened = crate::secure::unseal(
        std::str::from_utf8(&sealed).context("Doğrulanmış medya kaynak paketi metin değil")?,
    )?;
    let source: PinnedMediaSource =
        serde_json::from_str(&opened).context("Yenilenen doğrulanmış medya kaynak paketi bozuk")?;
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
                anyhow!("Mevcut HLS/DASH parçalarının uyumluluk kanıtı yok veya bozuk; eserler korundu. Açık yeniden başlatma gerekir: {error:#}")
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
                    "Yenilenen HLS/DASH kaynağının atomik geçiş kaydı bulunamadı; eserler korundu",
                )?,
            )
            .context("Yenilenen HLS/DASH kaynağının atomik geçiş kaydı bozuk")?;
            if pending.version != MEDIA_SOURCE_STATE_VERSION
                || pending.previous_request != current.request
                || pending.next.request != fingerprint
                || pending.next.evidence != *evidence
                || pending.next.evidence.live
            {
                bail!("Yenilenen HLS/DASH kaynak/akış/parça çizelgesi mevcut çalışmayla eşleşmiyor; eserler korundu. Açık yeniden başlatma gerekir");
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
                bail!("Mevcut çalışma HLS/DASH parça kanıtı içeriyor ancak yeni kaynak tam parça çizelgesi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir");
            }
            if fs::read_to_string(workspace.join(".request"))? != fingerprint {
                bail!("Bu hedefin geçici dosyaları başka bir indirmeye ait. Yeni bir dosya adı seçin.");
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
        bail!("Çalışma klasörünün SSDownload'a ait olduğu doğrulanamadı");
    }
    let parent = path.parent().context("Çalışma klasörü eksik")?;
    if fs::symlink_metadata(parent)?.file_attributes() & 0x400 != 0 {
        bail!("Çalışma klasörü yönlendirme içeriyor");
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
    let mut removed = 0usize;
    let mut bytes = 0u64;
    for entry in fs::read_dir(&plan.workspace)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_attributes() & 0x400 != 0 {
            bail!("Çalışma klasörü yönlendirme içeriyor");
        }
        if !meta.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if !name.starts_with(&stem) || keep.contains(&path) {
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
                bail!("Çalışma klasörü yönlendirme içeriyor");
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
        bail!("Kaynak hazırlığı durduruldu");
    }
    if selection_fingerprint(previous)? != selection_fingerprint(next)? {
        bail!("Kaynak yenilemesi seçilen görüntü/ses/altyazı kimliğini değiştiremez");
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
        anyhow!("Mevcut HLS/DASH parçalarının kalıcı kaynak/akış/parça kanıtı yok; eserler korundu. Açık yeniden başlatma gerekir: {error:#}")
    })?;
    let previous_fingerprint = request_fingerprint(previous)?;
    if current.request != previous_fingerprint
        || current.evidence.selection != selection_fingerprint(previous)?
    {
        bail!("Mevcut HLS/DASH çalışma alanı önceki iş ve seçim kimliğiyle eşleşmiyor; eserler korundu");
    }
    if current.evidence.live {
        bail!("Canlı HLS/DASH çizelgesi zamanla değişebildiği için mevcut parçalar yenilenen kaynakla birleştirilemez; eserler korundu. Açık yeniden başlatma gerekir");
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
        bail!("Kaynak hazırlığı durduruldu");
    }
    validate_track_selection(next, &info)?;
    validate_container_selection(next, &info)?;
    let selector = format_selector(next, &info)?;
    let capture = capture_media_source(next, &inspection, &info, &selector, control, network)?;
    let next_evidence = capture.evidence.context(
        "Yenilenen kaynak desteklenen, tam ve bütçe içinde doğrulanmış bir sonlu HLS/DASH parça çizelgesi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir",
    )?;
    let pinned = capture.pinned.context(
        "Yenilenen kaynak indiriciye sabitlenebilen HLS/DASH parça bilgisi sunmuyor; eserler korundu. Açık yeniden başlatma gerekir",
    )?;
    if current.evidence != next_evidence {
        bail!("Yenilenen HLS/DASH kaynağı seçilen akışların parça kimlik veya zaman çizelgesini değiştirdi; eserler korundu. Açık yeniden başlatma gerekir");
    }
    if control.stop_requested() {
        bail!("Kaynak hazırlığı durduruldu");
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
        bail!("HLS/DASH kaynak yenileme geçiş kaydı mevcut çalışma alanıyla eşleşmiyor")
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
