//! Durable publication journal and per-job output ownership.
use crate::{
    model::{Job, TransferResult},
    paths::AppPaths,
    secure,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Storage::FileSystem::*,
};

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    relative: PathBuf,
    identity: [u32; 3],
}
#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    owner: String,
    source: PathBuf,
    target: PathBuf,
    identity: [u32; 3],
    bytes: u64,
    directory: bool,
    files: Vec<Entry>,
}
fn wide(path: &Path) -> Vec<u16> {
    crate::winpath::wide_long(path)
}
fn receipt_path(paths: &AppPaths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context(crate::i18n::ui(
        "İş kimliği geçersiz",
        "The job id is invalid",
    ))?;
    Ok(paths
        .base_dir
        .join("completions")
        .join(format!("{id}.json")))
}
fn file_identity(path: &Path) -> Result<[u32; 3]> {
    reject_link(path)?;
    unsafe {
        let handle = CreateFileW(
            wide(path).as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        let error = std::io::Error::last_os_error();
        CloseHandle(handle);
        if ok == 0 {
            return Err(error.into());
        }
        Ok([
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ])
    }
}
fn reject_link(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Dosya yolu yönlendirme içeriyor: {0}",
                "The file path contains a redirection: {0}",
                path.display()
            )
        );
    }
    Ok(())
}
/// `reject_link` for a folder that may be removed meanwhile: `cleanup_work` of another job takes
/// the folders it leaves empty, so a path that vanishes between a look and the check is absent.
fn reject_link_if_present(path: &Path) -> Result<()> {
    match reject_link(path) {
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(())
        }
        other => other,
    }
}
/// Creates `temp` and fills it; a failed fill removes the partial file.
fn write_temp(temp: &Path, fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(temp)?;
    let filled = fill(&mut file);
    drop(file);
    if let Err(error) = filled {
        let _ = fs::remove_file(temp);
        return Err(error.into());
    }
    Ok(())
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context(crate::i18n::ui(
        "Kayıt klasörü eksik",
        "The record folder is missing",
    ))?;
    fs::create_dir_all(parent)?;
    // Rust filesystem calls support long paths, but raw Win32 calls require
    // the extended-length prefix for both sides of the atomic replacement.
    let parent = parent.canonicalize()?;
    let path = parent.join(path.file_name().context(crate::i18n::ui(
        "Kayıt dosya adı eksik",
        "Record filename is missing",
    ))?);
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    write_temp(&temp, |file| {
        file.write_all(bytes)?;
        file.sync_all()
    })?;
    let mut attempt = 0u32;
    loop {
        let ok = unsafe {
            MoveFileExW(
                wide(&temp).as_ptr(),
                wide(&path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if attempt < crate::output::LOCK_RETRIES && crate::output::is_transient_lock(&error) {
            attempt += 1;
            std::thread::sleep(crate::output::LOCK_RETRY_STEP * attempt);
            continue;
        }
        let _ = fs::remove_file(temp);
        return Err(error.into());
    }
}
fn read_receipt(paths: &AppPaths, id: &str) -> Result<Option<Receipt>> {
    let path = receipt_path(paths, id)?;
    if !path.exists() {
        return Ok(None);
    }
    let value: Receipt = serde_json::from_str(&secure::unseal(&fs::read_to_string(path)?)?)?;
    if value.owner != id {
        bail!(
            "{}",
            crate::i18n::ui(
                "Tamamlama kaydının sahibi eşleşmiyor",
                "The completion record owner does not match"
            )
        );
    }
    Ok(Some(value))
}
pub(crate) fn recovered(paths: &AppPaths, id: &str) -> Result<Option<TransferResult>> {
    let Some(value) = read_receipt(paths, id)? else {
        return Ok(None);
    };
    if value.target.exists() && file_identity(&value.target)? == value.identity {
        for entry in &value.files {
            if file_identity(&value.target.join(&entry.relative))? != entry.identity {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Tamamlanan playlist dosyası değişmiş",
                        "The completed playlist file has changed"
                    )
                );
            }
        }
        return Ok(Some(TransferResult {
            path: value.target,
            bytes: value.bytes,
            warning: None,
        }));
    }
    if value.source.exists()
        && !value.target.exists()
        && file_identity(&value.source)? == value.identity
    {
        let _lock = crate::output::OutputLock::acquire(&value.target)?;
        if unsafe {
            MoveFileExW(
                wide(&value.source).as_ptr(),
                wide(&value.target).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        return recovered(paths, id);
    }
    if !value.source.exists() {
        bail!("{}", crate::i18n::ui("Tamamlama kaydındaki dosya kayıp/değişmiş; otomatik yeniden indirme durduruldu", "The file in the completion record is missing or changed; automatic re-download was stopped"));
    }
    Ok(None)
}
pub(crate) fn claim(path: &Path, id: &str) -> Result<()> {
    let marker = claim_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .context(crate::i18n::ui(
            "Bu çıktı adı başka bir iş için ayrılmış",
            "This output name is reserved for another job",
        ))?;
    file.write_all(id.as_bytes())?;
    file.sync_all()?;
    unsafe {
        SetFileAttributesW(wide(&marker).as_ptr(), FILE_ATTRIBUTE_HIDDEN);
    }
    Ok(())
}
pub(crate) fn claim_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".ssdownload.claim");
    PathBuf::from(name)
}
pub(crate) fn release(path: &Path, id: &str) -> Result<()> {
    let marker = claim_path(path);
    if !marker.exists() {
        return Ok(());
    }
    if fs::read_to_string(&marker)? != id {
        bail!(
            "{}",
            crate::i18n::ui(
                "Çıktı rezervasyonu başka bir işe ait",
                "The output reservation belongs to another job"
            )
        )
    }
    fs::remove_file(marker)?;
    Ok(())
}
pub(crate) fn work_dir(output: &Path, id: &str) -> PathBuf {
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".ssdownload-work")
        .join("jobs")
        .join(id)
}
/// `fs::create_dir_all` for a folder below `.ssdownload-work`. `cleanup_work` of another job in
/// the folder removes the folders it leaves empty, so a level that vanishes while the path is
/// being created is created again.
pub(crate) fn create_work_dirs(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            fs::create_dir_all(path)
        } else {
            Err(error)
        }
    })
}
pub(crate) fn prepare_work(job: &Job) -> Result<PathBuf> {
    let path = job.work_dir.as_ref().context(crate::i18n::ui(
        "İş çalışma klasörü eksik",
        "The job work folder is missing",
    ))?;
    for ancestor in path.ancestors().take(3) {
        reject_link_if_present(ancestor)?;
    }
    let marker = path.join(".job");
    if path.exists() {
        reject_link(path)?;
        match fs::read_to_string(&marker) {
            Ok(owner) if owner == job.id => {}
            Ok(_) => bail!(
                "{}",
                crate::i18n::ui(
                    "Çalışma dosyaları başka bir işe ait",
                    "The work files belong to another job"
                )
            ),
            // A crash between creating the folder and writing its marker leaves an empty
            // folder; nothing in it can belong to another job, so it is adopted. A folder
            // with content but no marker stays untouched.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::read_dir(path)?.next().is_some() {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Çalışma klasörünün sahipliği kanıtlanamadı; dosyalar korundu",
                            "Ownership of the work folder could not be proven; files were kept"
                        )
                    )
                }
                atomic_write(&marker, job.id.as_bytes())?;
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        create_work_dirs(path)?;
        atomic_write(&marker, job.id.as_bytes())?;
    }
    if let Some(root) = path.ancestors().nth(2) {
        let attributes = fs::symlink_metadata(root)?.file_attributes();
        unsafe {
            SetFileAttributesW(wide(root).as_ptr(), attributes | FILE_ATTRIBUTE_HIDDEN);
        }
    }
    Ok(path.join(job.path.file_name().context(crate::i18n::ui(
        "Dosya adı eksik",
        "The file name is missing",
    ))?))
}
fn inventory(root: &Path, path: &Path, entries: &mut Vec<Entry>) -> Result<()> {
    reject_link(path)?;
    if path.is_dir() {
        for child in fs::read_dir(path)? {
            inventory(root, &child?.path(), entries)?;
        }
    } else {
        entries.push(Entry {
            relative: path.strip_prefix(root)?.to_owned(),
            identity: file_identity(path)?,
        });
    }
    Ok(())
}
pub(crate) fn publish(
    paths: &AppPaths,
    job: &Job,
    result: TransferResult,
) -> Result<TransferResult> {
    let mut entries = Vec::new();
    let directory = result.path.is_dir();
    if directory {
        inventory(&result.path, &result.path, &mut entries)?;
    }
    let desired = job
        .path
        .parent()
        .context(crate::i18n::ui(
            "Hedef klasör eksik",
            "The target folder is missing",
        ))?
        .join(result.path.file_name().context(crate::i18n::ui(
            "Çıktı adı eksik",
            "The output name is missing",
        ))?);
    let identity = file_identity(&result.path)?;
    for index in 0..10000 {
        let target = if index == 0 {
            desired.clone()
        } else {
            crate::output::numbered(&desired, index)
        };
        if target.exists() {
            continue;
        }
        let marker = claim_path(&target);
        let ours = fs::read_to_string(&marker)
            .map(|s| s == job.id)
            .unwrap_or(false);
        if !ours && claim(&target, &job.id).is_err() {
            continue;
        }
        let receipt = Receipt {
            owner: job.id.clone(),
            source: result.path.clone(),
            target: target.clone(),
            identity,
            bytes: result.bytes,
            directory,
            files: entries.clone(),
        };
        // The target of an earlier try that failed to move the file. The journal below replaces
        // it, so nothing would release its claim.
        let earlier = read_receipt(paths, &job.id)
            .ok()
            .flatten()
            .map(|earlier| earlier.target)
            .filter(|earlier| *earlier != target);
        atomic_write(
            &receipt_path(paths, &job.id)?,
            secure::seal(serde_json::to_string(&receipt)?)?.as_bytes(),
        )?;
        if let Some(earlier) = earlier {
            let _ = release(&earlier, &job.id);
        }
        if unsafe {
            MoveFileExW(
                wide(&result.path).as_ptr(),
                wide(&target).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } != 0
        {
            return Ok(TransferResult {
                path: target,
                bytes: result.bytes,
                warning: result.warning,
            });
        }
        let error = std::io::Error::last_os_error();
        if !target.exists() {
            // The journal names this target; the claim keeps the name for the retry in `recovered`.
            return Err(error).context(crate::i18n::ui(
                "Son dosya taşınamadı",
                "The final file could not be moved",
            ));
        }
        release(&target, &job.id)?;
    }
    bail!(
        "{}",
        crate::i18n::ui("Boş çıktı adı bulunamadı", "No free output name was found")
    )
}
/// Names in a work folder that are not a download's result: the owner marker, temporaries of
/// `atomic_write`, the media workspace, the transfer's part, state and segment files, the
/// output a browser transfer stages while it remuxes. The `browser-transfer` folder and its
/// `browser-transfer-previous-<uuid>` archives are none of them: they keep `adopt_finished`
/// from adopting anything.
fn is_work_artifact(name: &str) -> bool {
    name == ".job"
        || name == ".ssdownload-work"
        || (name.starts_with('.') && name.ends_with(".tmp"))
        || name.contains(".ssdownload.")
        || name.starts_with(".browser-mux-")
}

/// `name` in lower case without the number that `output::numbered` adds: `Movie (2)` is `movie`.
fn without_number(name: &str) -> String {
    let name = name.to_lowercase();
    match name
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
    {
        Some((base, number))
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base.to_owned()
        }
        _ => name,
    }
}

/// The name of a file without its extension and without its number: `Movie (2).mkv` and
/// `movie.mp4` have the same stem.
fn result_stem(name: &str) -> String {
    let path = crate::output::without_real_extension(Path::new(name));
    without_number(&path.to_string_lossy())
}

/// Whether the downloader of `job` can leave a file called `name` as its result. When the
/// request fixes the file's name, that is the job's name, numbered, or with the extension of
/// the media container; otherwise the server names the file.
fn is_possible_result(job: &Job, name: &str) -> bool {
    job.request.filename.is_none()
        || job.path.file_name().is_some_and(|own| {
            let own = own.to_string_lossy();
            let stem = result_stem(name);
            // The last dot of a title without an extension ("Talk v2.0") reads as an extension, but
            // the container's extension is added to the whole name ("Talk v2.0.mp4").
            stem == result_stem(&own) || stem == without_number(&own)
        })
}

/// Publishes the finished result of a download that stopped before `publish` wrote its journal
/// entry. A download first moves its result out of its part files into the work folder
/// (`output::publish`) and `publish` journals and moves it on; between the two nothing names the
/// result, `recovered` finds nothing and the download would start over. The result is the one
/// regular file of the work folder that the job's downloader can have left (`is_possible_result`)
/// and that is not an artifact of the transfer. Nothing is adopted when the folder holds
/// anything else, a folder included. The `browser-transfer` folder is one: its receiver puts the
/// file in the work folder first and verifies it after, so a file beside the folder may have
/// failed that verification. So is a `browser-transfer-previous-<uuid>` archive: a restart
/// archives the folder before it moves the earlier output into it, so a file beside an archive
/// can be the output the restart discards. The caller has checked the folder with `prepare_work`.
pub(crate) fn adopt_finished(paths: &AppPaths, job: &Job) -> Result<Option<TransferResult>> {
    let Some(work) = &job.work_dir else {
        return Ok(None);
    };
    let mut finished = None;
    for entry in fs::read_dir(work)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_work_artifact(&name) {
            continue;
        }
        if !entry.file_type()?.is_file()
            || !is_possible_result(job, &name)
            || finished.replace(entry.path()).is_some()
        {
            return Ok(None);
        }
    }
    let Some(path) = finished else {
        return Ok(None);
    };
    reject_link(&path)?;
    let bytes = fs::metadata(&path)?.len();
    publish(
        paths,
        job,
        TransferResult {
            path,
            bytes,
            warning: None,
        },
    )
    .map(Some)
}

pub(crate) fn cleanup_work(job: &Job) -> Result<()> {
    let Some(path) = &job.work_dir else {
        return Ok(());
    };
    if path.exists() {
        reject_link(path)?;
        // A crash before the marker was written leaves an empty folder, which is nobody else's.
        let owned = match fs::read_to_string(path.join(".job")) {
            Ok(owner) => owner == job.id,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::read_dir(path)?.next().is_none()
            }
            Err(error) => return Err(error.into()),
        };
        if !owned {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Çalışma klasörü sahipliği doğrulanamadı",
                    "Work folder ownership could not be verified"
                )
            )
        }
        let mut entries = Vec::new();
        inventory(path, path, &mut entries)?;
        fs::remove_dir_all(path)?;
    }
    let root = path.ancestors().nth(2).filter(|root| {
        root.file_name()
            .is_some_and(|name| name == ".ssdownload-work")
    });
    // A source renewal that restarted the download moved the previous parts here
    // (see `archive_refresh_artifacts`); nothing else ever deletes them.
    if let Some(archives) = path
        .ancestors()
        .nth(2)
        .map(|root| root.join("refresh-archives").join(&job.id))
        .filter(|archives| archives.exists())
    {
        reject_link(&archives)?;
        fs::remove_dir_all(archives)?;
    }
    // The folders of the other jobs and the media workspaces keep theirs: removing a folder that
    // is not empty fails and is left as it is.
    if let Some(root) = root {
        for folder in [
            root.join("refresh-archives"),
            root.join("jobs"),
            root.to_path_buf(),
        ] {
            let _ = fs::remove_dir(folder);
        }
    }
    Ok(())
}
pub(crate) fn remove(paths: &AppPaths, job: &Job, delete_final: bool) -> Result<()> {
    if delete_final {
        if let Some(receipt) = read_receipt(paths, &job.id)? {
            if receipt.target.exists() {
                if file_identity(&receipt.target)? != receipt.identity {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Son dosya değişmiş; silinmedi",
                            "The final file has changed; it was not deleted"
                        )
                    )
                }
                if receipt.directory {
                    for entry in &receipt.files {
                        let file = receipt.target.join(&entry.relative);
                        if !file.exists() {
                            continue;
                        }
                        if file_identity(&file)? != entry.identity {
                            bail!(
                                "{}",
                                crate::i18n::ui(
                                    "Playlist dosyası değişmiş; silinmedi",
                                    "The playlist file has changed; it was not deleted"
                                )
                            )
                        }
                        fs::remove_file(file)?;
                    }
                    fs::remove_dir(&receipt.target).context(crate::i18n::ui(
                        "Klasörde başka dosyalar var; klasör korundu",
                        "The folder contains other files; it was kept",
                    ))?;
                } else {
                    fs::remove_file(&receipt.target)?;
                }
            }
        } else if job.path.exists()
            && (job.state == crate::model::JobState::Completed || job.legacy_completed)
        {
            reject_link(&job.path)?;
            if job.path.is_dir() {
                bail!("{}", crate::i18n::ui("Eski playlist'in sahiplik kaydı yok; klasörden elle silin", "The old playlist has no ownership record; delete it from the folder manually"))
            }
            fs::remove_file(&job.path)?;
        }
    }
    cleanup_work(job)?;
    if let Some(path) = &job.claim {
        release(path, &job.id)?;
    }
    release(&job.path, &job.id)?;
    if let Some(receipt) = read_receipt(paths, &job.id)? {
        release(&receipt.target, &job.id)?;
    }
    Ok(())
}
pub(crate) fn acknowledge(paths: &AppPaths, job: &Job) -> Result<()> {
    if let Some(path) = &job.claim {
        release(path, &job.id)?;
    }
    release(&job.path, &job.id)?;
    if let Some(receipt) = read_receipt(paths, &job.id)? {
        release(&receipt.target, &job.id)?;
    }
    cleanup_work(job)
}

/// Points a publication journal at the renamed final file of its job.
pub(crate) fn retarget(paths: &AppPaths, id: &str, target: &Path) -> Result<()> {
    let Some(mut receipt) = read_receipt(paths, id)? else {
        return Ok(());
    };
    receipt.target = target.to_path_buf();
    atomic_write(
        &receipt_path(paths, id)?,
        secure::seal(serde_json::to_string(&receipt)?)?.as_bytes(),
    )
}

pub(crate) fn forget(paths: &AppPaths, id: &str) -> Result<()> {
    if read_receipt(paths, id)?.is_some() {
        fs::remove_file(receipt_path(paths, id)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AddRequest, JobState};

    fn fixture() -> (AppPaths, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ssdownload-recovery-test-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        fs::create_dir_all(root.join("out")).unwrap();
        (paths, root)
    }

    fn job(root: &Path, id: String, filename: &str) -> Job {
        let path = root.join("out").join(filename);
        Job {
            id: id.clone(),
            request: AddRequest {
                url: "https://example.test/file".into(),
                ..Default::default()
            },
            name: filename.into(),
            state: JobState::Downloading,
            path: path.clone(),
            downloaded: 0,
            total: None,
            speed: 0,
            eta: None,
            error: None,
            phase: "İndiriliyor".into(),
            created_at: 1,
            updated_at: 1,
            attempts: 1,
            work_dir: Some(work_dir(&path, &id)),
            remove_requested: None,
            claim: Some(path),
            legacy_completed: false,
            browser_transfer_authorized: false,
            priority: 0,
            force_start: false,
            open_when_done: false,
            live: false,
            sequence: 0,
            ranges: None,
            resumable: None,
        }
    }

    #[test]
    fn claims_prevent_overwrite_and_only_owner_can_release() {
        let (_, root) = fixture();
        let output = root.join("out").join("file.bin");
        let owner = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        claim(&output, &owner).unwrap();
        assert!(claim(&output, &other).is_err());
        assert!(release(&output, &other).is_err());
        release(&output, &owner).unwrap();
        assert!(!claim_path(&output).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_receipt_recovers_final_file_then_acknowledges_ownership() {
        let (paths, root) = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let job = job(&root, id.clone(), "movie.mp4");
        claim(&job.path, &id).unwrap();
        let scratch = prepare_work(&job).unwrap();
        fs::write(&scratch, b"movie bytes").unwrap();

        let published = publish(
            &paths,
            &job,
            TransferResult {
                path: scratch,
                bytes: 11,
                warning: None,
            },
        )
        .unwrap();
        assert_eq!(published.path, job.path);
        assert_eq!(fs::read(&published.path).unwrap(), b"movie bytes");
        let recovered_value = recovered(&paths, &id).unwrap().unwrap();
        assert_eq!(recovered_value.path, published.path);
        assert_eq!(recovered_value.bytes, 11);

        acknowledge(&paths, &job).unwrap();
        assert!(!claim_path(&job.path).exists());
        assert!(!job.work_dir.as_ref().unwrap().exists());
        assert!(published.path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_finished_job_leaves_no_work_folder_in_the_download_folder_once_no_job_uses_it() {
        let (paths, root) = fixture();
        let out = root.join("out");
        let started = |filename: &str| {
            let job = job(&root, uuid::Uuid::new_v4().to_string(), filename);
            claim(&job.path, &job.id).unwrap();
            let scratch = prepare_work(&job).unwrap();
            fs::write(&scratch, b"bytes").unwrap();
            (job, scratch)
        };
        let finish = |(job, scratch): &(Job, PathBuf)| {
            let result = TransferResult {
                path: scratch.clone(),
                bytes: 5,
                warning: None,
            };
            publish(&paths, job, result).unwrap();
            acknowledge(&paths, job).unwrap();
        };
        let first = started("one.mp4");
        let second = started("two.mp4");

        finish(&first);
        assert_eq!(
            names_in(&out),
            [".ssdownload-work", "one.mp4", "two.mp4.ssdownload.claim"]
        );
        assert!(second.0.work_dir.as_ref().unwrap().exists());

        finish(&second);
        assert_eq!(names_in(&out), ["one.mp4", "two.mp4"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removing_a_job_releases_its_name_and_leaves_no_work_folder() {
        let (paths, root) = fixture();
        let job = job(&root, uuid::Uuid::new_v4().to_string(), "movie.mp4");
        claim(&job.path, &job.id).unwrap();
        let scratch = prepare_work(&job).unwrap();
        fs::write(append_state(&scratch), b"{}").unwrap();
        let out = root.join("out");
        // A failed job keeps its name and its work folder for the retry.
        assert_eq!(
            names_in(&out),
            [".ssdownload-work", "movie.mp4.ssdownload.claim"]
        );

        remove(&paths, &job, false).unwrap();
        assert_eq!(names_in(&out), Vec::<String>::new());
        fs::remove_dir_all(root).unwrap();
    }

    /// A directory junction `link` to `target`, which needs no privilege.
    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
    }

    #[test]
    fn a_work_folder_another_job_removed_meanwhile_is_absent_and_a_redirection_is_refused() {
        let (_, root) = fixture();
        let missing = root.join("out").join(".ssdownload-work").join("jobs");
        reject_link_if_present(&missing).unwrap();
        reject_link_if_present(&root.join("out")).unwrap();

        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let link = root.join("out").join("link");
        junction(&link, &target);
        let refused = reject_link_if_present(&link).unwrap_err();
        assert!(
            format!("{refused:#}").contains(&link.display().to_string()),
            "{refused:#}"
        );
        // A redirection whose target is gone is still a redirection.
        fs::remove_dir(&target).unwrap();
        assert!(reject_link_if_present(&link).is_err());

        let id = uuid::Uuid::new_v4().to_string();
        let job = job(&root, id, "movie.mp4");
        let redirected = root.join("out").join(".ssdownload-work");
        junction(&redirected, &root);
        assert!(
            prepare_work(&job).is_err(),
            "a work folder behind a redirection is refused"
        );
        fs::remove_dir(&redirected).unwrap();
        prepare_work(&job).unwrap();
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    /// A job whose download stopped after moving its result into the work folder and before
    /// `publish` wrote the journal: the result sits at `path`, nothing names it.
    fn stopped_before_the_journal(root: &Path, filename: &str, bytes: &[u8]) -> (Job, PathBuf) {
        let id = uuid::Uuid::new_v4().to_string();
        let job = job(root, id.clone(), filename);
        claim(&job.path, &id).unwrap();
        let result = prepare_work(&job).unwrap();
        fs::write(&result, bytes).unwrap();
        (job, result)
    }

    #[test]
    fn a_finished_file_the_work_folder_kept_without_a_journal_is_published() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        // The transfer's own state file outlives its part file when it stops after the rename.
        fs::write(append_state(&result), b"{}").unwrap();
        assert!(recovered(&paths, &job.id).unwrap().is_none());

        let adopted = adopt_finished(&paths, &job).unwrap().unwrap();
        assert_eq!(
            (adopted.path.as_path(), adopted.bytes),
            (job.path.as_path(), 11)
        );
        assert_eq!(fs::read(&job.path).unwrap(), b"movie bytes");
        assert!(!result.exists());
        // From here on the journal names the file, as after any other publication.
        assert_eq!(recovered(&paths, &job.id).unwrap().unwrap().path, job.path);

        acknowledge(&paths, &job).unwrap();
        assert!(!job.work_dir.as_ref().unwrap().exists());
        assert!(!claim_path(&job.path).exists());
        assert_eq!(fs::read(&job.path).unwrap(), b"movie bytes");
        fs::remove_dir_all(root).unwrap();
    }

    fn append_state(result: &Path) -> PathBuf {
        let mut name = result.as_os_str().to_os_string();
        name.push(".ssdownload.state");
        PathBuf::from(name)
    }

    #[test]
    fn a_folder_of_the_work_folder_is_never_published_as_the_result() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "playlist", b"unused");
        let work = job.work_dir.clone().unwrap();
        fs::remove_file(&result).unwrap();
        // A finished playlist folder is not recognised; the job downloads it again.
        fs::create_dir_all(result.join("part")).unwrap();
        fs::write(result.join("001 - one.mp4"), b"one").unwrap();
        assert!(adopt_finished(&paths, &job).unwrap().is_none());
        assert!(result.join("001 - one.mp4").exists() && !job.path.exists());
        fs::remove_dir_all(&result).unwrap();

        // What a restart of a browser transfer leaves when the app stops between renaming the
        // transfer's folder to an archive and creating it again.
        let archive = work.join(format!(
            "browser-transfer-previous-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(archive.join("streams")).unwrap();
        fs::write(archive.join("streams").join("1.bin"), b"bytes").unwrap();
        assert!(adopt_finished(&paths, &job).unwrap().is_none());
        assert!(archive.join("streams").join("1.bin").exists() && !job.path.exists());
        assert!(recovered(&paths, &job.id).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_beside_the_live_folder_of_a_browser_transfer_is_left_to_its_receiver() {
        let (paths, root) = fixture();
        let (mut job, result) = stopped_before_the_journal(&root, "movie.mp4", b"unverified");
        let work = job.work_dir.clone().unwrap();
        // The receiver places the file in the work folder and verifies it after; a file that
        // failed the check stays there beside the receiver's folder.
        let live = work.join("browser-transfer");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("stream-00000000.bin"), b"unverified").unwrap();

        for filename in [None, Some("movie.mp4".to_owned())] {
            job.request.filename = filename;
            assert!(adopt_finished(&paths, &job).unwrap().is_none());
            assert!(result.exists() && !job.path.exists());
            assert!(recovered(&paths, &job.id).unwrap().is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_output_a_restart_of_a_browser_transfer_discards_is_not_published() {
        let (paths, root) = fixture();
        let (mut job, result) = stopped_before_the_journal(&root, "movie.mp4", b"old output");
        let work = job.work_dir.clone().unwrap();
        // A restart renames the transfer's folder to an archive, creates the folder again and
        // only then moves the earlier output into the archive; a stop between the first two
        // steps leaves the output beside the archive.
        let archive = work.join(format!(
            "browser-transfer-previous-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("stream-00000000.bin"), b"old output").unwrap();

        for filename in [None, Some("movie.mp4".to_owned())] {
            job.request.filename = filename;
            assert!(adopt_finished(&paths, &job).unwrap().is_none());
            assert!(result.exists() && !job.path.exists());
            assert!(recovered(&paths, &job.id).unwrap().is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_output_a_stopped_remux_staged_is_never_published() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"unused");
        let work = job.work_dir.clone().unwrap();
        fs::remove_file(&result).unwrap();
        // `place_browser_file` writes the remuxed copy under this name before it renames it to the
        // job's name; the request leaves the file's name to the server, so any name is possible.
        let staged = work.join(format!(".browser-mux-{}.mp4", uuid::Uuid::new_v4()));
        fs::write(&staged, b"half a file").unwrap();

        assert!(job.request.filename.is_none());
        assert!(adopt_finished(&paths, &job).unwrap().is_none());
        assert!(staged.exists() && !job.path.exists());
        assert!(recovered(&paths, &job.id).unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_a_name_the_downloader_can_give_is_published_when_the_request_fixes_the_name() {
        let (paths, root) = fixture();
        // Each name gets a job of its own: `stem` is the stem of the job's name.
        let adopt = |index: usize, found: &dyn Fn(&str) -> String| {
            let stem = format!("clip{index}");
            let (mut job, result) =
                stopped_before_the_journal(&root, &format!("{stem}.mp4"), b"bytes");
            job.request.filename = Some(format!("{stem}.mp4"));
            fs::rename(&result, result.with_file_name(found(&stem))).unwrap();
            adopt_finished(&paths, &job).unwrap()
        };
        let rejected: [&dyn Fn(&str) -> String; 5] = [
            &|_| "other.bin".to_owned(),
            &|stem| format!("{stem}2.mp4"),
            &|stem| format!("my {stem}.mp4"),
            &|stem| format!("{stem} (x).mp4"),
            &|stem| format!("{stem} (1) (2).mp4"),
        ];
        for (index, found) in rejected.into_iter().enumerate() {
            assert!(adopt(index, found).is_none(), "rejected name {index}");
        }
        // The name, numbered, or with the extension of another container.
        let accepted: [&dyn Fn(&str) -> String; 4] = [
            &|stem| format!("{stem}.mp4"),
            &|stem| format!("{} (1).mp4", stem.to_uppercase()),
            &|stem| format!("{stem}.mkv"),
            &|stem| format!("{stem} (12).webm"),
        ];
        for (index, found) in accepted.into_iter().enumerate() {
            let adopted = adopt(10 + index, found).unwrap_or_else(|| panic!("name {index}"));
            assert_eq!(adopted.bytes, 5);
            assert!(adopted.path.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_numbered_tar_archive_is_a_name_the_downloader_can_give() {
        let (paths, root) = fixture();
        let adopt = |index: usize, found: &dyn Fn(&str) -> String| {
            let stem = format!("bundle{index}");
            let name = format!("{stem}.tar.gz");
            let (mut job, result) = stopped_before_the_journal(&root, &name, b"bytes");
            job.request.filename = Some(name);
            fs::rename(&result, result.with_file_name(found(&stem))).unwrap();
            adopt_finished(&paths, &job).unwrap()
        };
        assert!(
            adopt(0, &|stem| format!("{stem}2.tar.gz")).is_none(),
            "another name"
        );
        let adopted = adopt(1, &|stem| format!("{stem} (1).tar.gz")).expect("numbered archive");
        assert_eq!(adopted.bytes, 5);
        assert!(adopted.path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_title_with_a_dot_and_no_extension_is_published_under_its_container() {
        let (paths, root) = fixture();
        // The title's last dot reads as the extension `.0`; the container's extension follows it.
        let adopt = |index: usize, found: &dyn Fn(&str) -> String| {
            let title = format!("Talk v{index}.0");
            let (mut job, result) = stopped_before_the_journal(&root, &title, b"bytes");
            job.request.filename = Some(title.clone());
            fs::rename(&result, result.with_file_name(found(&title))).unwrap();
            adopt_finished(&paths, &job).unwrap()
        };
        let rejected: [&dyn Fn(&str) -> String; 3] = [
            &|title| format!("my {title}.mp4"),
            &|title| format!("{title}1.mp4"),
            &|title| format!("{title} (x).mp4"),
        ];
        for (index, found) in rejected.into_iter().enumerate() {
            assert!(adopt(index, found).is_none(), "rejected name {index}");
        }
        let accepted: [&dyn Fn(&str) -> String; 4] = [
            &|title| format!("{title}.mp4"),
            &|title| format!("{} (2).mkv", title.to_uppercase()),
            &|title| title.to_owned(),
            &|title| format!("{}.mp4", title.trim_end_matches(".0")),
        ];
        for (index, found) in accepted.into_iter().enumerate() {
            let adopted = adopt(10 + index, found).unwrap_or_else(|| panic!("name {index}"));
            assert_eq!(adopted.bytes, 5);
            assert!(adopted.path.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn any_name_is_published_when_the_server_may_choose_it() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "download.php", b"report bytes");
        fs::rename(&result, result.with_file_name("report.pdf")).unwrap();
        let adopted = adopt_finished(&paths, &job).unwrap().unwrap();
        assert_eq!(adopted.path.file_name().unwrap(), "report.pdf");
        assert_eq!(fs::read(&adopted.path).unwrap(), b"report bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_media_workspace_a_stop_left_beside_the_result_does_not_hide_it() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        // The workspace is removed after the result is published and may outlive a stop.
        let workspace = result
            .parent()
            .unwrap()
            .join(".ssdownload-work")
            .join("owner");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("media.mp4"), b"movie bytes").unwrap();

        let adopted = adopt_finished(&paths, &job).unwrap().unwrap();
        assert_eq!(adopted.path, job.path);
        acknowledge(&paths, &job).unwrap();
        assert!(!workspace.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn work_that_is_not_one_finished_result_is_left_alone() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        let work = job.work_dir.clone().unwrap();
        fs::remove_file(&result).unwrap();
        // Replaces everything but the owner marker with `entries` (name, is a folder).
        let left_alone = |entries: &[(&str, bool)]| {
            for entry in fs::read_dir(&work).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().unwrap() == ".job" {
                    continue;
                }
                if path.is_dir() {
                    fs::remove_dir_all(&path).unwrap();
                } else {
                    fs::remove_file(&path).unwrap();
                }
            }
            for (name, folder) in entries {
                if *folder {
                    fs::create_dir_all(work.join(name)).unwrap();
                } else {
                    fs::write(work.join(name), b"bytes").unwrap();
                }
            }
            assert!(
                adopt_finished(&paths, &job).unwrap().is_none(),
                "{entries:?}"
            );
            assert!(!job.path.exists(), "published from {entries:?}");
            for (name, _) in entries {
                assert!(work.join(name).exists(), "{name} was lost");
            }
        };

        left_alone(&[]);
        left_alone(&[(".0b3f9c1e.tmp", false)]);
        left_alone(&[("movie.mp4.ssdownload.part", false)]);
        left_alone(&[("movie.mp4.ssdownload.state", false)]);
        left_alone(&[("movie.mp4.ssdownload.part.000", false)]);
        left_alone(&[(".ssdownload-work", true)]);
        left_alone(&[("browser-transfer", true)]);
        left_alone(&[("browser-transfer-previous-0b3f9c1e", true)]);
        // Two results cannot be told apart.
        left_alone(&[("movie.mp4", false), ("movie (1).mp4", false)]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_publication_does_not_take_a_name_another_job_claimed() {
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        // Another job holds the free name `movie (2).mp4` and the first two exist or are ours.
        fs::write(root.join("out").join("movie (1).mp4"), b"user file").unwrap();
        let other = uuid::Uuid::new_v4().to_string();
        claim(&root.join("out").join("movie (2).mp4"), &other).unwrap();
        // The job's own claim on `movie.mp4` is released, and another job took that name too.
        release(&job.path, &job.id).unwrap();
        claim(&job.path, &other).unwrap();

        let published = publish(
            &paths,
            &job,
            TransferResult {
                path: result,
                bytes: 11,
                warning: None,
            },
        )
        .unwrap();
        assert_eq!(published.path.file_name().unwrap(), "movie (3).mp4");
        for name in ["movie.mp4", "movie (2).mp4"] {
            let marker = claim_path(&root.join("out").join(name));
            assert_eq!(fs::read_to_string(marker).unwrap(), other, "{name}");
            assert!(!root.join("out").join(name).exists(), "{name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_publication_whose_move_failed_keeps_its_claim_until_the_retry_moves_the_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        fs::write(&job.path, b"user file").unwrap();
        let target = root.join("out").join("movie (1).mp4");
        let other = uuid::Uuid::new_v4().to_string();
        // A program that shares the file for reading only blocks the rename.
        let scanner = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&result)
            .unwrap();
        assert!(adopt_finished(&paths, &job).is_err());
        assert!(!target.exists());
        // The journal names the target; no other job can reserve it in the meantime.
        assert_eq!(fs::read_to_string(claim_path(&target)).unwrap(), job.id);
        assert!(claim(&target, &other).is_err());

        drop(scanner);
        let done = recovered(&paths, &job.id).unwrap().unwrap();
        assert_eq!(done.path, target);
        assert_eq!(fs::read(&target).unwrap(), b"movie bytes");
        assert_eq!(fs::read(&job.path).unwrap(), b"user file");
        assert_eq!(fs::read_to_string(claim_path(&target)).unwrap(), job.id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_claim_kept_for_a_name_a_file_took_before_the_retry_is_released_with_the_journal() {
        use std::os::windows::fs::OpenOptionsExt;
        let (paths, root) = fixture();
        let (job, result) = stopped_before_the_journal(&root, "movie.mp4", b"movie bytes");
        let out = root.join("out");
        fs::write(&job.path, b"user file").unwrap();
        let first = out.join("movie (1).mp4");
        // A program that shares the file for reading only blocks the first try.
        let scanner = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&result)
            .unwrap();
        assert!(adopt_finished(&paths, &job).is_err());
        assert_eq!(fs::read_to_string(claim_path(&first)).unwrap(), job.id);
        drop(scanner);

        // A file takes the name that the journal names, and another job reserves the next one.
        fs::write(&first, b"another file").unwrap();
        let other = uuid::Uuid::new_v4().to_string();
        claim(&out.join("movie (2).mp4"), &other).unwrap();
        assert!(recovered(&paths, &job.id).unwrap().is_none());
        let published = adopt_finished(&paths, &job).unwrap().unwrap();
        assert_eq!(published.path, out.join("movie (3).mp4"));
        assert!(!claim_path(&first).exists());
        assert_eq!(
            fs::read_to_string(claim_path(&out.join("movie (2).mp4"))).unwrap(),
            other
        );

        acknowledge(&paths, &job).unwrap();
        let left: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".claim"))
            .collect();
        assert_eq!(left, ["movie (2).mp4.ssdownload.claim"]);
        assert_eq!(fs::read(&first).unwrap(), b"another file");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn acknowledging_a_job_also_removes_its_source_refresh_archives() {
        let (paths, root) = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let job = job(&root, id.clone(), "movie.mp4");
        prepare_work(&job).unwrap();
        let work = job.work_dir.as_ref().unwrap();
        let archive = work
            .ancestors()
            .nth(2)
            .unwrap()
            .join("refresh-archives")
            .join(&id)
            .join("1700000000000-abc");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("movie.mp4.ssdownload.part"), b"old parts").unwrap();

        acknowledge(&paths, &job).unwrap();
        assert!(!work.exists());
        assert!(!work
            .ancestors()
            .nth(2)
            .unwrap()
            .join("refresh-archives")
            .join(&id)
            .exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_preserves_existing_file_by_numbering_new_output() {
        let (paths, root) = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let job = job(&root, id.clone(), "movie.mp4");
        fs::write(&job.path, b"existing user file").unwrap();
        let scratch = prepare_work(&job).unwrap();
        fs::write(&scratch, b"new movie").unwrap();

        let published = publish(
            &paths,
            &job,
            TransferResult {
                path: scratch,
                bytes: 9,
                warning: None,
            },
        )
        .unwrap();
        assert_eq!(published.path.file_name().unwrap(), "movie (1).mp4");
        assert_eq!(fs::read(&job.path).unwrap(), b"existing user file");
        assert_eq!(fs::read(&published.path).unwrap(), b"new movie");
        acknowledge(&paths, &job).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_job_can_be_removed_when_a_crash_left_its_work_folder_without_a_marker() {
        let (paths, root) = fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let job = job(&root, id, "movie.mp4");
        let work = job.work_dir.as_ref().unwrap();
        fs::create_dir_all(work).unwrap();

        remove(&paths, &job, false).unwrap();
        assert!(!work.exists());

        // Content without a marker cannot be proven to belong to this job.
        fs::create_dir_all(work).unwrap();
        fs::write(work.join("movie.mp4.ssdownload.part"), b"unknown").unwrap();
        assert!(remove(&paths, &job, false).is_err());
        assert!(work.join("movie.mp4.ssdownload.part").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_temporary_write_leaves_no_partial_file() {
        let (_, root) = fixture();
        let temp = root.join(".record.tmp");
        let failed = write_temp(&temp, |file| {
            file.write_all(b"partial")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(failed.is_err());
        assert!(!temp.exists());

        write_temp(&temp, |file| file.write_all(b"whole")).unwrap();
        assert_eq!(fs::read(&temp).unwrap(), b"whole");
        fs::remove_dir_all(root).unwrap();
    }
}
