use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The base directory of the first `AppPaths` constructed in this process.
/// Profile-scoped scratch (cookie jars) resolves through it so a `--data-dir`
/// profile never writes into the default location.
static ACTIVE_BASE: OnceLock<PathBuf> = OnceLock::new();

/// The queue database file name inside the profile directory.
pub(crate) const DATABASE_FILE: &str = "queue.sqlite3";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub base_dir: PathBuf,
    pub database: PathBuf,
    pub tools_dir: PathBuf,
    pub download_dir: PathBuf,
}
impl AppPaths {
    pub fn discover(override_dir: Option<&Path>) -> Result<Self> {
        Self::new(profile_base_dir(override_dir))
    }
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        let base_dir = if base_dir.is_absolute() {
            base_dir
        } else {
            std::env::current_dir()?.join(base_dir)
        };
        let _ = ACTIVE_BASE.set(base_dir.clone());
        let download_dir = default_download_dir(&base_dir);
        let value = Self {
            database: base_dir.join(DATABASE_FILE),
            tools_dir: tools_dir_of(&base_dir),
            base_dir,
            download_dir,
        };
        std::fs::create_dir_all(&value.base_dir).context(crate::i18n::ui(
            "Uygulama veri klasörü oluşturulamadı",
            "The application data folder could not be created",
        ))?;
        std::fs::create_dir_all(&value.tools_dir).context(crate::i18n::ui(
            "Medya bileşenleri klasörü oluşturulamadı",
            "The media components folder could not be created",
        ))?;
        Ok(value)
    }
    pub fn pipe_name(&self) -> String {
        use sha2::{Digest, Sha256};
        let path = self
            .base_dir
            .canonicalize()
            .unwrap_or_else(|_| self.base_dir.clone());
        let digest = Sha256::digest(path.to_string_lossy().to_lowercase().as_bytes());
        format!(r"\\.\pipe\SSDownload-{}", hex::encode(&digest[..12]))
    }
}

/// The folder of a profile that holds the media components the application installs.
pub(crate) fn tools_dir_of(base_dir: &Path) -> PathBuf {
    base_dir.join("tools")
}

/// Resolves the application profile directory without constructing `AppPaths`.
pub(crate) fn profile_base_dir(override_dir: Option<&Path>) -> PathBuf {
    override_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SSDOWNLOAD_HOME").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("SSDownload")
        })
}

/// The folder downloads are saved to when the settings name none: the known Downloads folder
/// of the user, or a folder in the profile when Windows has none. A test build always saves
/// inside the profile directory, which the test deletes, so tests never fill the real Downloads
/// folder.
fn default_download_dir(base_dir: &Path) -> PathBuf {
    let downloads = if cfg!(test) { None } else { known_downloads() };
    downloads
        .unwrap_or_else(|| base_dir.join("Downloads"))
        .join("SSDownload")
}

/// The directory that holds the cookie-jar folder: the profile of the app. A test build uses
/// a scratch folder of its process instead, because its tests build many profiles and delete
/// them, and none of them may reach the folder of the installed app.
fn cookie_jar_base(active: Option<&Path>) -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join(format!("ssdownload-test-{}", std::process::id()));
    }
    active
        .map(Path::to_path_buf)
        .unwrap_or_else(|| profile_base_dir(None))
}

/// Cookie jars passed to child processes rest in a profile subdirectory, created on demand.
pub fn cookie_jar_dir() -> Result<PathBuf> {
    let dir = cookie_jar_base(ACTIVE_BASE.get().map(PathBuf::as_path)).join("cookie-jar");
    std::fs::create_dir_all(&dir).context(crate::i18n::ui(
        "Çerez klasörü oluşturulamadı",
        "The cookie folder could not be created",
    ))?;
    Ok(dir)
}

fn known_downloads() -> Option<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath},
    };
    unsafe {
        let mut ptr = std::ptr::null_mut();
        if SHGetKnownFolderPath(&FOLDERID_Downloads, 0, std::ptr::null_mut(), &mut ptr) < 0 {
            return None;
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1
        }
        let path = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
            ptr, len,
        )));
        CoTaskMemFree(ptr.cast());
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root() -> PathBuf {
        std::env::temp_dir().join(format!("ssdownload-paths-test-{}", uuid::Uuid::new_v4()))
    }

    /// Tests add downloads without naming a folder; they must land in the profile the test
    /// deletes, never in the Downloads folder of whoever runs the tests.
    #[test]
    fn a_test_profile_saves_inside_its_own_directory_and_never_in_the_real_downloads() {
        let root = scratch_root();
        let paths = AppPaths::new(root.clone()).unwrap();
        assert!(
            paths.download_dir.starts_with(&root),
            "{}",
            paths.download_dir.display()
        );
        if let Some(real) = known_downloads() {
            assert!(
                !paths.download_dir.starts_with(&real),
                "{}",
                paths.download_dir.display()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_cookie_jars_of_a_test_build_stay_out_of_every_profile() {
        let root = scratch_root();
        let installed = profile_base_dir(None);
        for active in [Some(root.as_path()), None] {
            let base = cookie_jar_base(active);
            assert!(base.starts_with(std::env::temp_dir()), "{}", base.display());
            assert!(!base.starts_with(&root), "{}", base.display());
            assert!(!base.starts_with(&installed), "{}", base.display());
        }
    }
}
