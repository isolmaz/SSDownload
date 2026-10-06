use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0},
    Storage::FileSystem::MoveFileExW,
    System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
};

fn wide(path: &Path) -> Vec<u16> {
    crate::winpath::wide_long(path)
}

pub(crate) fn identity(path: &Path) -> Result<String> {
    let parent = path
        .parent()
        .context("Çıktı klasörü bulunamadı")?
        .canonicalize()?;
    let full = parent.join(path.file_name().context("Dosya adı eksik")?);
    Ok(hex::encode(Sha256::digest(
        full.to_string_lossy().to_lowercase().as_bytes(),
    )))
}

/// Nonblocking, process-wide reservation, released by Windows even after a crash.
pub(crate) struct OutputLock(HANDLE);
impl OutputLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let name: Vec<u16> = format!("Local\\SSDownload-output-{}", identity(path)?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("Çıktı kilidi açılamadı");
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait == WAIT_FAILED {
            // Capture before CloseHandle, which would overwrite the thread's last error.
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            let code = error.raw_os_error().unwrap_or(-1);
            bail!("Dosya kilidi beklemesi Windows hatasıyla sonuçlandı (Win32 hata kodu {code}: {error}).");
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                CloseHandle(handle);
            }
            bail!("Bu dosya başka bir SSDownload işlemi tarafından kullanılıyor.");
        }
        Ok(Self(handle))
    }
}
impl Drop for OutputLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// Same-volume atomic publication. Never pass MOVEFILE_REPLACE_EXISTING.
pub(crate) fn publish(source: &Path, desired: &Path) -> Result<PathBuf> {
    for index in 0..10_000 {
        let target = if index == 0 {
            desired.to_path_buf()
        } else {
            let stem = desired.file_stem().unwrap_or_default().to_string_lossy();
            let name = match desired.extension() {
                Some(ext) => format!("{stem} ({index}).{}", ext.to_string_lossy()),
                None => format!("{stem} ({index})"),
            };
            desired.with_file_name(name)
        };
        if unsafe { MoveFileExW(wide(source).as_ptr(), wide(&target).as_ptr(), 0) } != 0 {
            return Ok(target);
        }
        let error = std::io::Error::last_os_error();
        if !target.exists() {
            return Err(error).context("Tamamlanan dosya güvenle taşınamadı");
        }
    }
    bail!("Bu klasörde boş bir çıktı adı bulunamadı.")
}

pub(crate) fn numbered(path: &Path, index: usize) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem} ({index}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({index})"),
    };
    path.with_file_name(name)
}

pub(crate) fn require_space(output: &Path, required: u64) -> Result<()> {
    let parent = output.parent().context("Hedef klasör eksik")?;
    let mut available = 0u64;
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide(parent).as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if available < required {
        bail!(
            "Birleştirme için {} MiB boş alan gerekli, {} MiB mevcut. İndirilen parçalar korundu.",
            required.div_ceil(1024 * 1024),
            available / (1024 * 1024)
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssdownload-output-test-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publish_never_overwrites_an_existing_user_file() {
        let directory = TestDirectory::new();
        let desired = directory.0.join("video.mp4");
        let source = directory.0.join("temporary.part");
        std::fs::write(&desired, b"user file").unwrap();
        std::fs::write(&source, b"downloaded file").unwrap();

        let published = publish(&source, &desired).unwrap();
        assert_eq!(published.file_name().unwrap(), "video (1).mp4");
        assert_eq!(std::fs::read(&desired).unwrap(), b"user file");
        assert_eq!(std::fs::read(&published).unwrap(), b"downloaded file");
        assert!(!source.exists());
    }

    #[test]
    fn output_identity_is_stable_for_the_same_path() {
        let directory = TestDirectory::new();
        let path = directory.0.join("same-name.bin");
        assert_eq!(identity(&path).unwrap(), identity(&path).unwrap());
        assert_ne!(
            identity(&path).unwrap(),
            identity(&directory.0.join("other.bin")).unwrap()
        );
    }

    #[test]
    fn space_checks_and_publication_work_beyond_max_path() {
        let directory = TestDirectory::new();
        let deep = directory.0.join("x".repeat(120)).join("y".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let desired = deep.join("payload.bin");
        assert!(
            desired.to_string_lossy().chars().count() > 260,
            "test path must exceed MAX_PATH to exercise the prefix"
        );
        let source = directory.0.join("temporary.part");
        std::fs::write(&source, b"payload").unwrap();

        require_space(&desired, 7).unwrap();
        let published = publish(&source, &desired).unwrap();
        assert_eq!(std::fs::read(&published).unwrap(), b"payload");
    }
}
