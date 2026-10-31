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
pub(crate) fn prepare_work(job: &Job) -> Result<PathBuf> {
    let path = job.work_dir.as_ref().context(crate::i18n::ui(
        "İş çalışma klasörü eksik",
        "The job work folder is missing",
    ))?;
    for ancestor in path.ancestors().take(3) {
        if ancestor.exists() {
            reject_link(ancestor)?;
        }
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
        fs::create_dir_all(path)?;
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
        atomic_write(
            &receipt_path(paths, &job.id)?,
            secure::seal(serde_json::to_string(&receipt)?)?.as_bytes(),
        )?;
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
        release(&target, &job.id)?;
        if !target.exists() {
            return Err(error).context(crate::i18n::ui(
                "Son dosya taşınamadı",
                "The final file could not be moved",
            ));
        }
    }
    bail!(
        "{}",
        crate::i18n::ui("Boş çıktı adı bulunamadı", "No free output name was found")
    )
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> (AppPaths, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ssdownload-recovery-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
