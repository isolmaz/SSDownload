use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The base directory of the first `AppPaths` constructed in this process.
/// Profile-scoped scratch (cookie jars) resolves through it so a `--data-dir`
/// profile never writes into the default location.
static ACTIVE_BASE: OnceLock<PathBuf> = OnceLock::new();

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
        let download_dir = known_downloads()
            .unwrap_or_else(|| base_dir.join("Downloads"))
            .join("SSDownload");
        let value = Self {
            database: base_dir.join("queue.sqlite3"),
            tools_dir: base_dir.join("tools"),
            base_dir,
            download_dir,
        };
        std::fs::create_dir_all(&value.base_dir).context("Uygulama veri klasörü oluşturulamadı")?;
        std::fs::create_dir_all(&value.tools_dir)
            .context("Medya bileşenleri klasörü oluşturulamadı")?;
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

/// Resolves the application profile directory without constructing `AppPaths`.
fn profile_base_dir(override_dir: Option<&Path>) -> PathBuf {
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

/// Cookie jars passed to child processes rest in a profile subdirectory, created on demand.
pub fn cookie_jar_dir() -> Result<PathBuf> {
    let base = ACTIVE_BASE
        .get()
        .cloned()
        .unwrap_or_else(|| profile_base_dir(None));
    let dir = base.join("cookie-jar");
    std::fs::create_dir_all(&dir).context("Çerez klasörü oluşturulamadı")?;
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
