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
        .context(crate::i18n::ui(
            "Çıktı klasörü bulunamadı",
            "The output folder was not found",
        ))?
        .canonicalize()?;
    let full = parent.join(path.file_name().context(crate::i18n::ui(
        "Dosya adı eksik",
        "The file name is missing",
    ))?);
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
            return Err(std::io::Error::last_os_error()).context(crate::i18n::ui(
                "Çıktı kilidi açılamadı",
                "The output lock could not be opened",
            ));
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        if wait == WAIT_FAILED {
            // Capture before CloseHandle, which would overwrite the thread's last error.
            let error = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            let code = error.raw_os_error().unwrap_or(-1);
            bail!(
                "{}",
                crate::i18n::ui_owned!(
                    format!(
                        "Dosya kilidi beklemesi Windows hatasıyla sonuçlandı (Win32 hata kodu {code}: {error})."
                    ),
                    format!(
                        "Waiting for the file lock failed with a Windows error (Win32 error code {code}: {error})."
                    )
                )
            );
        }
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                CloseHandle(handle);
            }
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu dosya başka bir SSDownload işlemi tarafından kullanılıyor.",
                    "This file is in use by another SSDownload process."
                )
            );
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

/// How often a rename is retried while a scanner or indexer briefly holds the file.
pub(crate) const LOCK_RETRIES: u32 = 5;
pub(crate) const LOCK_RETRY_STEP: std::time::Duration = std::time::Duration::from_millis(100);

/// ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION: a competing handle
/// that usually closes within moments.
pub(crate) fn is_transient_lock(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

/// The part of a file name after its last dot when it can be a file extension. A dot inside a
/// title ("Mr. Robot", "Vol. 2 best of", "Python 3.12 guide") leaves text with spaces after
/// it, which is not an extension and must stay part of the name.
fn real_extension(path: &Path) -> Option<&str> {
    let extension = path.extension()?.to_str()?;
    ((1..=16).contains(&extension.chars().count()) && extension.chars().all(char::is_alphanumeric))
        .then_some(extension)
}

/// The path without its extension, or unchanged when the last dot belongs to the name.
pub(crate) fn without_real_extension(path: &Path) -> PathBuf {
    if real_extension(path).is_some() {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

/// Same-volume atomic publication. Never pass MOVEFILE_REPLACE_EXISTING.
pub(crate) fn publish(source: &Path, desired: &Path) -> Result<PathBuf> {
    for index in 0..10_000 {
        let target = if index == 0 {
            desired.to_path_buf()
        } else {
            numbered(desired, index)
        };
        let mut attempt = 0u32;
        loop {
            if unsafe { MoveFileExW(wide(source).as_ptr(), wide(&target).as_ptr(), 0) } != 0 {
                return Ok(target);
            }
            let error = std::io::Error::last_os_error();
            if target.exists() {
                break;
            }
            if attempt < LOCK_RETRIES && is_transient_lock(&error) {
                attempt += 1;
                std::thread::sleep(LOCK_RETRY_STEP * attempt);
                continue;
            }
            return Err(error).context(crate::i18n::ui(
                "Tamamlanan dosya güvenle taşınamadı",
                "The completed file could not be moved safely",
            ));
        }
    }
    bail!(
        "{}",
        crate::i18n::ui(
            "Bu klasörde boş bir çıktı adı bulunamadı.",
            "No free output name was found in this folder."
        )
    )
}

pub(crate) fn numbered(path: &Path, index: usize) -> PathBuf {
    let name = match real_extension(path) {
        Some(extension) => {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            format!("{stem} ({index}).{extension}")
        }
        None => format!(
            "{} ({index})",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    };
    path.with_file_name(name)
}

pub(crate) fn require_space(output: &Path, required: u64) -> Result<()> {
    let parent = output.parent().context(crate::i18n::ui(
        "Hedef klasör eksik",
        "The target folder is missing",
    ))?;
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
            "{}",
            crate::i18n::ui_owned!(
                format!(
                    "Yeterli boş alan yok: {} MiB gerekli, {} MiB mevcut. İndirilen parçalar korundu.",
                    required.div_ceil(1024 * 1024),
                    available / (1024 * 1024)
                ),
                format!(
                    "Not enough free space: {} MiB needed, {} MiB available. The downloaded parts were kept.",
                    required.div_ceil(1024 * 1024),
                    available / (1024 * 1024)
                ),
            )
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
    fn numbering_and_stripping_keep_dots_that_belong_to_the_title() {
        let name = |path: PathBuf| path.file_name().unwrap().to_string_lossy().into_owned();
        let dir = PathBuf::from(r"C:\d");
        assert_eq!(name(numbered(&dir.join("Mr. Robot"), 1)), "Mr. Robot (1)");
        assert_eq!(
            name(numbered(&dir.join("Vol. 2 best of"), 2)),
            "Vol. 2 best of (2)"
        );
        assert_eq!(name(numbered(&dir.join("report.pdf"), 1)), "report (1).pdf");
        assert_eq!(name(numbered(&dir.join("noext"), 3)), "noext (3)");
        assert_eq!(
            without_real_extension(&dir.join("Vol. 2 best of")),
            dir.join("Vol. 2 best of")
        );
        assert_eq!(
            without_real_extension(&dir.join("Python 3.12 guide")),
            dir.join("Python 3.12 guide")
        );
        assert_eq!(
            without_real_extension(&dir.join("list.m3u8")),
            dir.join("list")
        );
    }

    #[test]
    fn publish_numbers_an_extensionless_title_without_splitting_it() {
        let directory = TestDirectory::new();
        let desired = directory.0.join("Mr. Robot");
        std::fs::write(&desired, b"existing").unwrap();
        let source = directory.0.join("temporary.part");
        std::fs::write(&source, b"new").unwrap();
        let published = publish(&source, &desired).unwrap();
        assert_eq!(published.file_name().unwrap(), "Mr. Robot (1)");
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
