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
    uuid::Uuid::parse_str(id).context("İş kimliği geçersiz")?;
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
        bail!("Dosya yolu yönlendirme içeriyor: {}", path.display());
    }
    Ok(())
}
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Kayıt klasörü eksik")?;
    fs::create_dir_all(parent)?;
    // Rust filesystem calls support long paths, but raw Win32 calls require
    // the extended-length prefix for both sides of the atomic replacement.
    let parent = parent.canonicalize()?;
    let path = parent.join(path.file_name().context("Record filename is missing")?);
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let ok = unsafe {
        MoveFileExW(
            wide(&temp).as_ptr(),
            wide(&path).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(temp);
        return Err(error.into());
    }
    Ok(())
}
fn read_receipt(paths: &AppPaths, id: &str) -> Result<Option<Receipt>> {
    let path = receipt_path(paths, id)?;
    if !path.exists() {
        return Ok(None);
    }
    let value: Receipt = serde_json::from_str(&secure::unseal(&fs::read_to_string(path)?)?)?;
    if value.owner != id {
        bail!("Tamamlama kaydının sahibi eşleşmiyor");
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
                bail!("Tamamlanan playlist dosyası değişmiş");
            }
        }
        return Ok(Some(TransferResult {
            path: value.target,
            bytes: value.bytes,
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
        bail!("Tamamlama kaydındaki dosya kayıp/değişmiş; otomatik yeniden indirme durduruldu");
    }
    Ok(None)
}
pub(crate) fn claim(path: &Path, id: &str) -> Result<()> {
    let marker = claim_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .context("Bu çıktı adı başka bir iş için ayrılmış")?;
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
        bail!("Çıktı rezervasyonu başka bir işe ait")
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
    let path = job.work_dir.as_ref().context("İş çalışma klasörü eksik")?;
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
            Ok(_) => bail!("Çalışma dosyaları başka bir işe ait"),
            // A crash between creating the folder and writing its marker leaves an empty
            // folder; nothing in it can belong to another job, so it is adopted. A folder
            // with content but no marker stays untouched.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::read_dir(path)?.next().is_some() {
                    bail!("Çalışma klasörünün sahipliği kanıtlanamadı; dosyalar korundu")
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
    Ok(path.join(job.path.file_name().context("Dosya adı eksik")?))
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
        .context("Hedef klasör eksik")?
        .join(result.path.file_name().context("Çıktı adı eksik")?);
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
            });
        }
        let error = std::io::Error::last_os_error();
        release(&target, &job.id)?;
        if !target.exists() {
            return Err(error).context("Son dosya taşınamadı");
        }
    }
    bail!("Boş çıktı adı bulunamadı")
}
pub(crate) fn cleanup_work(job: &Job) -> Result<()> {
    let Some(path) = &job.work_dir else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    reject_link(path)?;
    if fs::read_to_string(path.join(".job"))? != job.id {
        bail!("Çalışma klasörü sahipliği doğrulanamadı")
    }
    let mut entries = Vec::new();
    inventory(path, path, &mut entries)?;
    fs::remove_dir_all(path)?;
    Ok(())
}
pub(crate) fn remove(paths: &AppPaths, job: &Job, delete_final: bool) -> Result<()> {
    if delete_final {
        if let Some(receipt) = read_receipt(paths, &job.id)? {
            if receipt.target.exists() {
                if file_identity(&receipt.target)? != receipt.identity {
                    bail!("Son dosya değişmiş; silinmedi")
                }
                if receipt.directory {
                    for entry in &receipt.files {
                        let file = receipt.target.join(&entry.relative);
                        if !file.exists() {
                            continue;
                        }
                        if file_identity(&file)? != entry.identity {
                            bail!("Playlist dosyası değişmiş; silinmedi")
                        }
                        fs::remove_file(file)?;
                    }
                    fs::remove_dir(&receipt.target)
                        .context("Klasörde başka dosyalar var; klasör korundu")?;
                } else {
                    fs::remove_file(&receipt.target)?;
                }
            }
        } else if job.path.exists()
            && (job.state == crate::model::JobState::Completed || job.legacy_completed)
        {
            reject_link(&job.path)?;
            if job.path.is_dir() {
                bail!("Eski playlist'in sahiplik kaydı yok; klasörden elle silin")
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
            },
        )
        .unwrap();
        assert_eq!(published.path.file_name().unwrap(), "movie (1).mp4");
        assert_eq!(fs::read(&job.path).unwrap(), b"existing user file");
        assert_eq!(fs::read(&published.path).unwrap(), b"new movie");
        acknowledge(&paths, &job).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
