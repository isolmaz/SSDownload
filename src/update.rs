//! Startup update check against the GitHub Releases feed.
//!
//! Feed contract (`<release-base>/version.json`):
//! `{"version":"1.4.1","url":"<Setup.exe URL>","sha256":"<hex>","notes":"<short text>"}`
//! The base URL is overridable for staging tests through `SSDOWNLOAD_UPDATE_FEED`.
//!
//! NO-DELETE POLICY: this module never deletes a file. Every artifact that a
//! normal flow would clean up (failed/partial downloads, the Setup after a
//! successful install, superseded artifacts) is MOVED into a timestamped
//! quarantine folder `<data-dir>\update-quarantine\YYYYMMDD-HHMMSS\` and the
//! folder path is surfaced to the user. The NSIS `/S` install overwriting
//! binaries in place is an overwrite, not a deletion, and is allowed.

use crate::{model::Settings, network::NetworkGovernor, paths::AppPaths, tools::USER_AGENT};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Release feed base for the published product: the assets of the newest
/// GitHub release of the project repository. Installed copies keep reading the base they
/// were built with: moving it strands them unless the old base keeps serving a public,
/// signed feed that points to a build with the new base.
const DEFAULT_FEED_BASE: &str = "https://github.com/isolmaz/SSDownload/releases/latest/download";
/// HTTP(S) response byte ceiling for the version feed document.
const FEED_LIMIT: usize = 64 * 1024;
/// Setup download ceiling. A complete product Setup is far below this; the cap
/// prevents a runaway response from filling the disk.
const SETUP_LIMIT: u64 = 1024 * 1024 * 1024;
/// Smallest plausible product Setup; a truncated or substituted response fails
/// this before the installer is launched.
const MIN_SETUP_BYTES: u64 = 128 * 1024;
/// Version feed document name, relative to the feed base.
const FEED_FILE: &str = "version.json";
/// Ed25519 public key (raw 32 bytes, base64) for `version.json.sig`. The
/// matching private key never enters the repository; `scripts/prepare-release.ps1`
/// signs the feed document with it.
const FEED_PUBLIC_KEY: &str = "WmDz4IJ2QgikLIsKy6TXelUYYpne70FkIBOrNKnIYJQ=";
/// Signature document ceiling (base64 of a 64-byte signature).
const SIGNATURE_LIMIT: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

/// Live state of the running update attempt. The attempt runs on its own
/// thread without a window, so the main window polls this instead of receiving
/// notifications from a hierarchy that does not exist here.
#[derive(Debug, Clone)]
pub struct UpdateProgress {
    pub version: String,
    /// Turkish phase text ("Güncelleme indiriliyor", ...).
    pub message: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

impl UpdateProgress {
    /// Whole-percent progress when the response carried its length.
    pub fn percent(&self) -> Option<u32> {
        let total = self.total.filter(|value| *value > 0)?;
        Some(((self.downloaded.min(total) * 100) / total) as u32)
    }
}

static UPDATE_PROGRESS: std::sync::Mutex<Option<UpdateProgress>> = std::sync::Mutex::new(None);

/// Current update attempt, `None` while no update is being downloaded,
/// verified or installed.
pub fn progress() -> Option<UpdateProgress> {
    UPDATE_PROGRESS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn publish_progress(value: Option<UpdateProgress>) {
    let mut slot = UPDATE_PROGRESS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *slot = value;
}

/// Result of a completed update flow, rendered into dialogs or tray messages.
#[derive(Debug, Default)]
pub(crate) struct UpdateOutcome {
    pub(crate) success: bool,
    /// User-facing text in the interface language. Includes the quarantine path when applicable.
    pub(crate) message: String,
    pub(crate) quarantine: Option<PathBuf>,
    /// True when the installer was deliberately skipped (dry-run mode, or a portable copy
    /// that is pointed to the release page); the application keeps running.
    pub(crate) dry_run: bool,
}

impl UpdateOutcome {
    /// The text of the result dialog: the message, followed by the quarantine folder unless the
    /// message already names it.
    pub(crate) fn dialog_text(&self) -> String {
        let mut text = self.message.clone();
        if let Some(folder) = &self.quarantine {
            let folder = folder.display().to_string();
            if !text.contains(&folder) {
                text.push_str(&format!(
                    "\n\n{}: {folder}",
                    crate::i18n::ui("Karantina klasörü", "Quarantine folder")
                ));
            }
        }
        text
    }
}

/// Release-version comparison for the update feed. Both sides must be a stable
/// `major.minor.patch` release: a prerelease or build suffix, a missing component or a
/// non-numeric part is not a release this product ships, so it never counts as newer and
/// the feed cannot offer one. Returns true only when `candidate` is strictly newer.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    let (Some(current), Some(candidate)) = (parse_semver(current), parse_semver(candidate)) else {
        return false;
    };
    candidate > current
}

fn parse_semver(value: &str) -> Option<(u64, u64, u64)> {
    // The version names the staged file and the skipped-version preference verbatim.
    if value.trim() != value {
        return None;
    }
    let mut parts = value.split('.');
    let major = numeric_component(parts.next()?)?;
    let minor = numeric_component(parts.next()?)?;
    let patch = numeric_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn numeric_component(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Feed base resolution: `SSDOWNLOAD_UPDATE_FEED` overrides the default
/// (staging hook, mirrors the `SSDOWNLOAD_NATIVE_CASE` pattern). A bare
/// `version.json` URL is also accepted.
pub fn feed_url() -> String {
    let base = std::env::var("SSDOWNLOAD_UPDATE_FEED")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_FEED_BASE.to_string());
    with_feed_file(base.trim_end_matches('/'))
}

/// Names the feed document once under `base`, which must not end in a slash.
fn with_feed_file(base: &str) -> String {
    if base.ends_with(FEED_FILE) {
        base.to_string()
    } else {
        format!("{base}/{FEED_FILE}")
    }
}

/// Fetches one feed document with the governed transfer stack.
///
/// `staging` selects the unsigned local test transport: the document is then read from
/// the loopback origin itself and never through a redirect, so nothing outside this
/// machine can answer for a staging feed. The signed production feed keeps its redirect
/// chain (the release host redirects to its asset store) and stays on HTTPS.
fn fetch_document(
    network: &NetworkGovernor,
    url: &str,
    limit: usize,
    staging: bool,
) -> Result<Vec<u8>> {
    let mut easy = network.wildcard_easy(&Default::default())?;
    restrict_transport(&mut easy, staging)?;
    easy.url(url)?;
    easy.useragent(USER_AGENT)?;
    if staging {
        easy.follow_location(false)?;
    } else {
        easy.follow_location(true)?;
        easy.max_redirections(10)?;
    }
    easy.fail_on_error(false)?;
    easy.connect_timeout(Duration::from_secs(15))?;
    easy.timeout(Duration::from_secs(30))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    let mut data = Vec::new();
    let mut too_large = false;
    let performed = {
        let mut transfer = easy.transfer();
        transfer.write_function(|chunk| {
            if data.len().saturating_add(chunk.len()) > limit {
                too_large = true;
                return Ok(0);
            }
            data.extend_from_slice(chunk);
            Ok(chunk.len())
        })?;
        transfer.perform()
    };
    // The refused write makes the transfer itself fail, so the size verdict has to come
    // first or an oversized document is reported as an unreachable feed.
    if too_large {
        crate::bail_code!(crate::error_codes::UPD_001);
    }
    performed.map_err(|error| {
        anyhow!(
            "{}",
            crate::i18n::ui_format!(
                "Güncelleme akışına erişilemedi: {error}",
                "The update feed could not be reached: {error}",
                error = error
            )
        )
    })?;
    let status = easy.response_code()?;
    if status == 404 {
        crate::bail_code!(crate::error_codes::UPD_001);
    }
    if !(200..300).contains(&status) {
        crate::bail_code!(
            crate::error_codes::UPD_001,
            "{}",
            crate::i18n::ui_format!(
                "Güncelleme akışı HTTP {status} döndürdü",
                "The update feed returned HTTP {status}",
                status = status
            )
        );
    }
    Ok(data)
}

/// The staging override, when it names a loopback HTTP base.
///
/// Staging exists so isolated tests need no signing key, which means an unsigned document
/// is trusted there: the base is therefore required to address this machine. A lookalike
/// host (`127.0.0.1.example.com`, `localhost.example.com`) or a URL carrying credentials
/// is not loopback and keeps the signed production path.
fn staging_base() -> Option<url::Url> {
    // Release builds never waive the signature: the unsigned loopback feed exists for
    // isolated tests of debug builds only.
    if !cfg!(debug_assertions) {
        return None;
    }
    let value = std::env::var("SSDOWNLOAD_UPDATE_FEED").ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    is_loopback_http_url(&url).then_some(url)
}

/// True while `SSDOWNLOAD_UPDATE_FEED` points at a loopback staging server; the
/// signature requirement is waived there so isolated tests need no key.
fn staging_feed() -> bool {
    staging_base().is_some()
}

/// The staging feed document as it is actually fetched: the override re-serialized from its
/// own parse, so the waiver decision and the request cannot disagree about the host. A raw
/// spelling that the two parsers read differently must not be fetched: `url` reads a
/// backslash inside the authority as a path separator, libcurl as userinfo, so the raw form
/// could be accepted as loopback and still be answered by another host.
fn staging_feed_url() -> Option<String> {
    let base = staging_base()?;
    Some(with_feed_file(base.as_str().trim_end_matches('/')))
}

/// True for `http://` URLs that address this machine, carrying no credentials.
/// `localhost` is accepted because Windows resolves it internally to the loopback address.
fn is_loopback_http_url(url: &url::Url) -> bool {
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Restricts the handle to the transport the current feed mode may use.
///
/// The signed production feed is HTTPS only, including every redirect, so a signed
/// document can never be fetched over plaintext; an HTTP(S) proxy from the environment
/// stays usable there, as a corporate network expects, because the signature is the
/// integrity contract. An unsigned staging feed is the exact opposite: loopback HTTP
/// only, it does not follow redirects at all, and it never uses a proxy. A proxy named
/// in the environment would otherwise answer a `http://127.0.0.1...` request itself
/// (libcurl sends it the absolute URL), so an origin outside this machine could speak
/// for an unsigned feed.
fn restrict_transport(easy: &mut crate::network::GovernedEasy, staging: bool) -> Result<()> {
    let protocol = if staging {
        curl_sys::CURLPROTO_HTTP
    } else {
        curl_sys::CURLPROTO_HTTPS
    } as std::os::raw::c_long;
    for option in [
        curl_sys::CURLOPT_PROTOCOLS,
        curl_sys::CURLOPT_REDIR_PROTOCOLS,
    ] {
        let code = unsafe { curl_sys::curl_easy_setopt(easy.raw(), option, protocol) };
        if code != curl_sys::CURLE_OK {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Güncelleme aktarım protokol sınırı uygulanamadı",
                    "The update transfer protocol limit could not be applied",
                )
            );
        }
    }
    if staging {
        // An explicit empty proxy already means "no proxy"; the bypass list covers a
        // proxy libcurl could still select from the environment.
        easy.proxy("")?;
        easy.noproxy("*")?;
    }
    Ok(())
}

/// Verifies `document` against a base64 Ed25519 signature from the release feed.
fn verify_feed_signature(document: &[u8], signature: &[u8]) -> Result<()> {
    let encoded = std::str::from_utf8(signature).context(crate::i18n::ui(
        "İmza belgesi UTF-8 değil",
        "The signature document is not UTF-8",
    ))?;
    let raw = STANDARD.decode(encoded.trim()).context(crate::i18n::ui(
        "İmza belgesi base64 değil",
        "The signature document is not base64",
    ))?;
    let key = STANDARD.decode(FEED_PUBLIC_KEY).context(crate::i18n::ui(
        "Gömülü açık anahtar geçersiz",
        "The embedded public key is invalid",
    ))?;
    let key: [u8; 32] = key.try_into().map_err(|_| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Gömülü açık anahtar 32 bayt değil",
                "The embedded public key is not 32 bytes"
            )
        )
    })?;
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&key).context(crate::i18n::ui(
        "Gömülü açık anahtar bir Ed25519 anahtarı değil",
        "The embedded public key is not an Ed25519 key",
    ))?;
    let signature = ed25519_dalek::Signature::from_slice(&raw).context(crate::i18n::ui(
        "İmza 64 baytlık bir Ed25519 imzası değil",
        "The signature is not a 64-byte Ed25519 signature",
    ))?;
    verifying.verify_strict(document, &signature).map_err(|_| {
        crate::error_codes::coded(
            crate::error_codes::UPD_002,
            crate::i18n::ui(
                "Güncelleme akışının imzası doğrulanamadı; güncelleme reddedildi",
                "The update feed signature could not be verified; the update was rejected",
            ),
        )
    })
}

fn fetch_feed(network: &NetworkGovernor) -> Result<UpdateManifest> {
    // The waiver decision and the request come from one parse: in staging mode the fetched
    // URL is the re-serialized form, so a raw spelling that the URL parser and libcurl read
    // differently cannot be validated as loopback and answered from somewhere else.
    let staging_url = staging_feed_url();
    let staging = staging_url.is_some();
    let url = staging_url.unwrap_or_else(feed_url);
    let data = fetch_document(network, &url, FEED_LIMIT, staging)?;
    if !staging {
        // Production feeds are signed and stay on HTTPS; staging is loopback and unsigned.
        let signature = fetch_document(network, &format!("{url}.sig"), SIGNATURE_LIMIT, false)?;
        verify_feed_signature(&data, &signature)?;
    }
    let mut manifest: UpdateManifest = serde_json::from_slice(&data).context(crate::i18n::ui(
        "Güncelleme akışı geçersiz JSON içeriyor",
        "The update feed does not contain valid JSON",
    ))?;
    if parse_semver(&manifest.version).is_none() {
        crate::bail_code!(crate::error_codes::UPD_001);
    }
    let Some(setup_url) = downloadable_url(&manifest.url, staging) else {
        crate::bail_code!(crate::error_codes::UPD_001);
    };
    manifest.url = setup_url;
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        crate::bail_code!(crate::error_codes::UPD_001);
    }
    Ok(manifest)
}

/// The URL libcurl must fetch for the Setup, re-serialized from its own parse so the URL
/// that was validated is exactly the URL that is fetched. `None` when the feed's URL is not
/// acceptable for the current feed mode: the production feed carries signed https:// URLs
/// only and they may redirect, because the signed document is the integrity contract, while
/// a staging feed is unsigned, so its Setup must be a loopback http:// URL - any local
/// address and port, never a remote origin.
fn downloadable_url(value: &str, staging: bool) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let accepted = if staging {
        is_loopback_http_url(&url)
    } else {
        url.scheme() == "https" && url.host().is_some()
    };
    if accepted {
        Some(url.to_string())
    } else {
        None
    }
}

/// Creates `<data-dir>\update-quarantine\<YYYYMMDD-HHMMSS>` and returns it.
/// Every superseded or rejected artifact moves here; nothing is ever deleted.
pub(crate) fn create_quarantine_folder(paths: &AppPaths) -> Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dir = paths
        .base_dir
        .join("update-quarantine")
        .join(stamp.to_string());
    fs::create_dir_all(&dir).with_context(|| {
        crate::i18n::ui_format!(
            "Karantina klasörü oluşturulamadı: {0}",
            "The quarantine folder could not be created: {0}",
            dir.display()
        )
    })?;
    Ok(dir)
}

/// NO-DELETE move: relocates `file` into a timestamped quarantine folder and
/// returns the folder. A cross-volume move falls back to copy. The original is
/// consumed by the move; nothing is ever deleted.
pub(crate) fn quarantine_file(paths: &AppPaths, file: &Path) -> Result<PathBuf> {
    let dir = create_quarantine_folder(paths)?;
    move_into_quarantine(&dir, file)?;
    Ok(dir)
}

/// Moves one artifact into an existing quarantine folder.
fn move_into_quarantine(folder: &Path, file: &Path) -> Result<()> {
    let destination = folder.join(file.file_name().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui("Geçersiz dosya adı", "Invalid file name")
        )
    })?);
    if fs::rename(file, &destination).is_err() {
        fs::copy(file, &destination).with_context(|| {
            crate::i18n::ui_format!(
                "Karantinaya kopyalama başarısız: {0}",
                "Copying to quarantine failed: {0}",
                file.display()
            )
        })?;
        retire_copied_original(file);
    }
    Ok(())
}

/// The rename into quarantine failed and the copy succeeded: the original is renamed to
/// `<name>.moved` (never deleted) so the startup reconciliation, which only picks staged
/// `SSDownload-*-Setup.exe` files, does not copy it again at every start. When the original is
/// still locked the rename fails too and the next start retries, which is the intent.
fn retire_copied_original(file: &Path) {
    let mut retired = file.as_os_str().to_owned();
    retired.push(".moved");
    let _ = fs::rename(file, PathBuf::from(retired));
}

/// Moves staged Setup downloads into a timestamped quarantine folder.
///
/// The installer helper must keep reading the staged Setup until the silent
/// install finishes, so the post-install move cannot happen in `run_update`.
/// This runs once at startup: any leftover the previous session could not move
/// (successful install, crashed helper) is relocated here and reported.
pub(crate) fn reconcile_update_staging(paths: &AppPaths) -> Result<Option<PathBuf>> {
    let staging = paths.base_dir.join("update-staging");
    let Ok(entries) = fs::read_dir(&staging) else {
        return Ok(None);
    };
    let mut leftovers = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Only this product's staged Setups move; foreign files stay untouched.
        if name.starts_with("SSDownload-") && name.ends_with("-Setup.exe") {
            leftovers.push(path);
        }
    }
    if leftovers.is_empty() {
        return Ok(None);
    }
    let folder = create_quarantine_folder(paths)?;
    for file in leftovers {
        if let Err(error) = move_into_quarantine(&folder, &file) {
            // A Setup that is still running by another process keeps its file
            // locked; the next start retries instead of failing startup.
            eprintln!("ssdownload update staging: {error:#}");
        }
    }
    Ok(Some(folder))
}

fn current_exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context(crate::i18n::ui(
        "Çalıştırılabilir dosya yolu alınamadı",
        "The executable path could not be obtained",
    ))?;
    Ok(exe
        .parent()
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui("Geçersiz çalıştırılabilir yolu", "Invalid executable path")
            )
        })?
        .to_path_buf())
}

/// Final sanity check on the staged Setup before it is launched: a viable
/// Windows executable image of plausible installer size. The feed's SHA-256
/// remains the integrity contract and the feed is the version authority.
fn staged_setup_is_plausible(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() < MIN_SETUP_BYTES {
        return false;
    }
    let mut header = [0u8; 64];
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() || &header[..2] != b"MZ" {
        return false;
    }
    let offset =
        u32::from_le_bytes([header[0x3c], header[0x3d], header[0x3e], header[0x3f]]) as u64;
    if offset + 4 > metadata.len() {
        return false;
    }
    if file.seek(std::io::SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature).is_ok() && &signature == b"PE\0\0"
}

fn download_setup(
    manifest: &UpdateManifest,
    destination: &Path,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(u64, Option<u64>),
    staging: bool,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(destination).with_context(|| {
        crate::i18n::ui_format!(
            "İndirme dosyası oluşturulamadı: {0}",
            "The download file could not be created: {0}",
            destination.display()
        )
    })?;
    let mut easy = network.wildcard_easy(&Default::default())?;
    restrict_transport(&mut easy, staging)?;
    easy.url(&manifest.url)?;
    easy.useragent(USER_AGENT)?;
    // A staging Setup is unsigned: it stays on the loopback origin and never redirects.
    // The production URL may redirect, and the signed SHA-256 remains its contract.
    if staging {
        easy.follow_location(false)?;
    } else {
        easy.follow_location(true)?;
        easy.max_redirections(10)?;
    }
    easy.fail_on_error(false)?;
    easy.connect_timeout(Duration::from_secs(30))?;
    easy.timeout(Duration::from_secs(60 * 60))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    easy.progress(true)?;
    let hasher = std::cell::RefCell::new(Sha256::new());
    let seen = std::cell::Cell::new(0u64);
    let oversized = std::cell::Cell::new(false);
    let io_error = std::cell::RefCell::new(None);
    {
        let mut transfer = easy.transfer();
        transfer.write_function(|chunk| {
            if seen.get().saturating_add(chunk.len() as u64) > SETUP_LIMIT {
                oversized.set(true);
                return Ok(0);
            }
            if let Err(error) = file.write_all(chunk) {
                *io_error.borrow_mut() = Some(error);
                return Ok(0);
            }
            hasher.borrow_mut().update(chunk);
            seen.set(seen.get() + chunk.len() as u64);
            Ok(chunk.len())
        })?;
        transfer.progress_function(|total, downloaded, _, _| {
            progress(
                downloaded.max(0.0) as u64,
                if total.is_finite() && total > 0.0 {
                    Some(total as u64)
                } else {
                    None
                },
            );
            true
        })?;
        if let Err(error) = transfer.perform() {
            if oversized.get() {
                crate::bail_code!(crate::error_codes::UPD_003);
            }
            if let Some(io_error) = io_error.borrow_mut().take() {
                return Err(io_error).context(crate::i18n::ui(
                    "Güncelleme diske yazılamadı",
                    "The update could not be written to disk",
                ));
            }
            return Err(anyhow!(error)).context(crate::i18n::ui(
                "Güncelleme indirmesi başarısız",
                "The update download failed",
            ));
        }
    }
    if oversized.get() {
        crate::bail_code!(crate::error_codes::UPD_003);
    }
    let status = easy.response_code()?;
    if !(200..300).contains(&status) {
        crate::bail_code!(
            crate::error_codes::UPD_003,
            "{}",
            crate::i18n::ui_format!(
                "Güncelleme sunucusu HTTP {status} döndürdü",
                "The update server returned HTTP {status}",
                status = status
            )
        );
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    let hasher = hasher.into_inner();
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(anyhow::Error::new(DigestMismatch {
            expected: manifest.sha256.clone(),
            actual,
        })
        .context(crate::error_codes::CodedError::new(
            crate::error_codes::UPD_003,
            crate::i18n::ui(
                "SHA-256 doğrulaması başarısız",
                "SHA-256 verification failed",
            ),
        )));
    }
    Ok(())
}

/// The root of the error of a finished download whose bytes hash to something other than the
/// digest of the signed feed. `run_update` words it with `digest_mismatch_message` once it
/// knows whether the file could be moved to quarantine.
#[derive(Debug)]
struct DigestMismatch {
    expected: String,
    actual: String,
}

impl std::fmt::Display for DigestMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&crate::i18n::ui_format!(
            "beklenen {0}, alınan {1}",
            "expected {0}, received {1}",
            self.expected,
            self.actual
        ))
    }
}

impl std::error::Error for DigestMismatch {}

/// The dialog text for a staged Setup whose digest differs from the signed feed.
fn digest_mismatch_message(expected: &str, actual: &str, quarantine: Option<&Path>) -> String {
    let moved = quarantine
        .map(|folder| {
            crate::i18n::ui_format!("\n\nDosya şu klasöre taşındı (kendiniz inceleyip silebilirsiniz):\n{0}", "\n\nThe file was moved to this folder (you can inspect and delete it yourself):\n{0}", folder.display())
        })
        .unwrap_or_default();
    crate::error_codes::coded(
        crate::error_codes::UPD_003,
        crate::i18n::ui_format!("SHA-256 özeti eşleşmedi. İndirilen kurulum dosyası güvenli değil.\nBeklenen: {expected}\nAlınan: {actual}{moved}", "SHA-256 digest does not match. The downloaded installer is not safe.\nExpected: {expected}\nReceived: {actual}{moved}", actual = actual, expected = expected, moved = moved),
    )
    .to_string()
}

/// Name of the marker the install helper leaves in the profile when the silent Setup did not
/// complete; the next start reports it.
const INSTALL_FAILURE_MARKER: &str = "update-failure.json";

/// The PowerShell the detached helper runs. It waits for this exact PID, verifies and runs the
/// staged Setup with `/S`, and in every outcome reopens the existing application: a failed or
/// timed-out installation leaves a marker file instead of an application that closed and never
/// came back.
fn install_helper_script(
    pid: u32,
    setup: &Path,
    target: &Path,
    expected_sha256: &str,
    marker: &Path,
    version: &str,
) -> String {
    // PowerShell also ends a single-quoted string at the typographic quotes U+2018-U+201B (a
    // user name such as "Ali’nin" has one); doubling any of them keeps it as text.
    let quoted = |value: &str| {
        let mut text = String::from("'");
        for character in value.chars() {
            if matches!(
                character,
                '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}'
            ) {
                text.push(character);
            }
            text.push(character);
        }
        text.push('\'');
        text
    };
    // Keep the verified file open without write/delete sharing through installation.
    // A second path-based hash check alone would leave another replacement race.
    format!(
        "$ErrorActionPreference='Stop'; \
         $failure=$null; \
         try {{ \
           $running=Get-Process -Id {pid} -ErrorAction SilentlyContinue; \
           if ($running) {{ $running | Wait-Process -Timeout 300 -ErrorAction Stop }}; \
           $file=[IO.File]::Open({setup},[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read); \
           try {{ \
             $hasher=[Security.Cryptography.SHA256]::Create(); \
             try {{ $hash=[BitConverter]::ToString($hasher.ComputeHash($file)).Replace('-','') }} \
             finally {{ $hasher.Dispose() }}; \
             if ($hash -ne {sha}) {{ throw 'Setup SHA-256 verification failed; installation was not started.' }}; \
             $installer=Start-Process -FilePath {setup} -ArgumentList '/S' -Wait -PassThru; \
             if ($installer.ExitCode -ne 0) {{ throw \"Setup exited with code $($installer.ExitCode)\" }} \
           }} finally {{ if ($file) {{ $file.Dispose() }} }} \
         }} catch {{ $failure=$_.Exception.Message }}; \
         if ($failure) {{ \
           try {{ [IO.File]::WriteAllText({marker},(ConvertTo-Json -Compress @{{version={version}; reason=$failure}}),(New-Object Text.UTF8Encoding $false)) }} catch {{ }} \
         }}; \
         Start-Process -FilePath {target}",
        setup = quoted(&setup.display().to_string()),
        target = quoted(&target.display().to_string()),
        sha = quoted(expected_sha256),
        marker = quoted(&marker.display().to_string()),
        version = quoted(version),
    )
}

/// The hidden, windowless PowerShell invocation of `script`.
///
/// `std::process::Command` escapes embedded quotes the way the C runtime expects, which
/// `cmd.exe` reads literally. Passing the script through `-EncodedCommand` avoids command-line
/// quoting entirely, and PowerShell's `Wait-Process`/`Start-Process` need no console (the
/// earlier `cmd` chain used `timeout`, which fails under `DETACHED_PROCESS` and silently
/// stopped before the restart).
fn helper_command(script: &str) -> std::process::Command {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let encoded = STANDARD.encode(utf16);
    let mut command = std::process::Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-EncodedCommand",
        &encoded,
    ]);
    command
}

/// Launches the detached install+restart helper and exits through the
/// graceful quit path. The helper waits for this process to exit, runs the
/// staged Setup with `/S`, then reopens the application. The installer cannot
/// replace a running `ssdownload.exe`, and its own `--quit` call plus a fixed
/// sleep is not a reliable barrier, so the helper waits for this exact PID.
fn launch_install_helper(
    paths: &AppPaths,
    setup: &Path,
    app_dir: &Path,
    manifest: &UpdateManifest,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let script = install_helper_script(
        std::process::id(),
        setup,
        &app_dir.join("ssdownload.exe"),
        &manifest.sha256,
        &paths.base_dir.join(INSTALL_FAILURE_MARKER),
        &manifest.version,
    );
    helper_command(&script)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|error| {
            crate::error_codes::coded(
                crate::error_codes::UPD_004,
                crate::i18n::ui_format!(
                    "Kurulum yardımcısı başlatılamadı: {error}",
                    "The setup helper could not be started: {error}",
                    error = error
                ),
            )
        })?;
    Ok(())
}

#[derive(Deserialize)]
struct InstallFailure {
    #[serde(default)]
    version: String,
    #[serde(default)]
    reason: String,
}

/// Text for a silent installation that did not complete, read from the helper's marker.
/// The marker moves into a quarantine folder (NO-DELETE policy), so it is reported once.
fn take_install_failure(paths: &AppPaths) -> Option<String> {
    let marker = paths.base_dir.join(INSTALL_FAILURE_MARKER);
    let bytes = fs::read(&marker).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}').trim();
    let failure = serde_json::from_str::<InstallFailure>(text).unwrap_or_else(|_| InstallFailure {
        version: String::new(),
        reason: text.chars().take(500).collect(),
    });
    if let Err(error) = quarantine_file(paths, &marker) {
        crate::logging::record(
            crate::logging::Event::warn("update.failure_marker").detail(format!("{error:#}")),
        );
    }
    let version = if failure.version.is_empty() {
        String::new()
    } else {
        format!(" {}", failure.version)
    };
    let reason = if failure.reason.is_empty() {
        crate::i18n::ui("bilinmeyen neden", "unknown reason").to_owned()
    } else {
        failure.reason
    };
    Some(crate::i18n::ui_format!("SSDownload{version} güncellemesi kurulamadı; mevcut sürüm yeniden başlatıldı.\n\nNeden: {reason}\n\nGüncellemeyi Yardım menüsünden yeniden deneyebilirsiniz.", "The SSDownload{version} update could not be installed; the current version was restarted.\n\nReason: {reason}\n\nYou can retry the update from the Help menu.", reason = reason, version = version))
}

/// Reports a failed silent installation from the previous run, if the helper left a marker.
fn report_install_failure(paths: &AppPaths) {
    let Some(message) = take_install_failure(paths) else {
        return;
    };
    crate::logging::record(
        crate::logging::Event::warn("update.install_failed").detail(message.replace('\n', " ")),
    );
    let spawned = std::thread::Builder::new()
        .name("update-failure-report".into())
        .spawn(move || {
            crate::gui::report_update_outcome(&UpdateOutcome {
                success: false,
                message,
                ..UpdateOutcome::default()
            });
        });
    if let Err(error) = spawned {
        crate::logging::record(crate::logging::Event::warn("update.check_spawn").detail(
            crate::i18n::ui_format!(
                "Kurulum hatası bildirimi başlatılamadı: {error}",
                "The install failure notice could not be started: {error}",
                error = error
            ),
        ));
    }
}

/// Registry key the installer writes; its `InstallLocation` is the installed copy's folder.
const INSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\SSDownload";
/// Page of the newest release: its notes and the portable ZIP.
pub(crate) const RELEASES_PAGE: &str = "https://github.com/isolmaz/SSDownload/releases/latest";

fn installed_location() -> Option<PathBuf> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(INSTALL_KEY)
        .ok()?;
    let value: String = key.get_value("InstallLocation").ok()?;
    let value = value.trim().trim_matches('"');
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// Comparable form of a directory: canonical when it exists, without the verbatim prefix and
/// the trailing separator, lower case (Windows paths are case-insensitive).
fn comparable_directory(path: &Path) -> String {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    text.trim_end_matches(['\\', '/']).to_lowercase()
}

/// True when `exe_dir` is the folder the installer registered. A copy that is not (the
/// portable ZIP, a developer build) cannot be updated by running the Setup.
fn is_install_folder(exe_dir: &Path, installed: Option<&Path>) -> bool {
    installed
        .is_some_and(|installed| comparable_directory(exe_dir) == comparable_directory(installed))
}

/// True when the running executable lives in the folder the installer registered. A portable
/// copy (ZIP, developer build) is updated from the release page, never by the silent Setup.
pub fn is_installed_copy() -> bool {
    current_exe_dir()
        .map(|dir| is_install_folder(&dir, installed_location().as_deref()))
        .unwrap_or(false)
}

/// Opens `url` in the default browser; false when the shell refused.
pub(crate) fn open_in_browser(url: &str) -> bool {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result as isize > 32
}

/// What a portable copy gets instead of the silent Setup: the release page, where the new
/// portable ZIP is. Marked `dry_run` so the caller does not quit the application: nothing
/// was installed.
fn portable_update_outcome(manifest: &UpdateManifest) -> UpdateOutcome {
    let opened = open_in_browser(RELEASES_PAGE);
    let mut message = crate::i18n::ui_format!("Bu kopya kurulu bir sürüm değil (taşınabilir). Taşınabilir kopya, yeni taşınabilir ZIP dosyası indirilerek güncellenir: SSDownload {0} paketini sürüm sayfasından indirip bu klasörün üzerine çıkarın.", "This copy is portable, not installed. A portable copy is updated by downloading the new portable ZIP: download the SSDownload {0} package from the release page and extract it over this folder.", manifest.version);
    message.push_str(&if opened {
        crate::i18n::ui_format!("\n\nSürüm sayfası tarayıcıda açıldı:\n{RELEASES_PAGE}", "\n\nThe release page was opened in your browser:\n{RELEASES_PAGE}", RELEASES_PAGE = RELEASES_PAGE)
    } else {
        crate::i18n::ui_format!("\n\nSürüm sayfası açılamadı; bu adresi tarayıcıya yapıştırın:\n{RELEASES_PAGE}", "\n\nThe release page could not be opened; paste this address into your browser:\n{RELEASES_PAGE}", RELEASES_PAGE = RELEASES_PAGE)
    });
    UpdateOutcome {
        success: true,
        message,
        quarantine: None,
        dry_run: true,
    }
}

pub(crate) struct UpdateContext<'a> {
    pub(crate) paths: &'a AppPaths,
    pub(crate) network: &'a NetworkGovernor,
    /// Dry-run: detection, dialog, download and verification run, but the
    /// installer helper is not launched. The staged Setup still lands in the
    /// quarantine folder per the NO-DELETE policy.
    pub(crate) dry_run: bool,
}

enum CheckOutcome {
    /// Feed unreachable or invalid — updater silently stands down.
    Silent,
    /// Feed version is not newer, or the user asked to skip it.
    NotNewer,
    Available(UpdateManifest),
}

fn check_with_feed(
    settings: &Settings,
    feed: Result<UpdateManifest, anyhow::Error>,
) -> CheckOutcome {
    let current = env!("CARGO_PKG_VERSION");
    let manifest = match feed {
        Ok(manifest) => manifest,
        Err(_) => return CheckOutcome::Silent,
    };
    if !is_newer(current, &manifest.version) {
        return CheckOutcome::NotNewer;
    }
    if settings.skipped_update_version.as_deref() == Some(manifest.version.as_str()) {
        return CheckOutcome::NotNewer;
    }
    CheckOutcome::Available(manifest)
}

/// Runs one full update attempt. On any failure the partial artifact is moved
/// into the quarantine folder and the error text includes that folder path.
pub(crate) fn run_update(ctx: &UpdateContext, manifest: &UpdateManifest) -> UpdateOutcome {
    // The Setup installs into the registered folder: for a portable copy it would add a
    // second, installed copy and the old portable exe would be restarted forever.
    if !ctx.dry_run && !is_installed_copy() {
        return portable_update_outcome(manifest);
    }
    let staging = staging_feed();
    let staging_dir = ctx.paths.base_dir.join("update-staging");
    let destination = staging_dir.join(format!("SSDownload-{}-Setup.exe", manifest.version));
    let version = manifest.version.clone();
    publish_progress(Some(UpdateProgress {
        version: version.clone(),
        message: crate::i18n::ui("Güncelleme indiriliyor", "Downloading the update").into(),
        downloaded: 0,
        total: None,
    }));
    match download_setup(
        manifest,
        &destination,
        ctx.network,
        &mut |downloaded, total| {
            publish_progress(Some(UpdateProgress {
                version: version.clone(),
                message: crate::i18n::ui("Güncelleme indiriliyor", "Downloading the update").into(),
                downloaded,
                total,
            }));
        },
        staging,
    ) {
        Ok(()) => {}
        Err(error) => {
            publish_progress(None);
            let quarantine = if fs::metadata(&destination).is_ok() {
                quarantine_file(ctx.paths, &destination).ok()
            } else {
                None
            };
            if let Some(mismatch) = error.downcast_ref::<DigestMismatch>() {
                return UpdateOutcome {
                    success: false,
                    message: digest_mismatch_message(
                        &mismatch.expected,
                        &mismatch.actual,
                        quarantine.as_deref(),
                    ),
                    quarantine,
                    dry_run: ctx.dry_run,
                };
            }
            let mut text = crate::i18n::ui_format!(
                "Güncelleme indirilemedi: {error:#}",
                "The update could not be downloaded: {error:#}",
                error = error
            );
            if let Some(folder) = &quarantine {
                text.push_str(&crate::i18n::ui_format!("\n\nKısmi indirme bu klasöre taşındı (istek üzerine inceleyip silebilirsiniz):\n{0}", "\n\nThe partial download was moved to this folder (you can review and delete it if you wish):\n{0}", folder.display()));
            }
            return UpdateOutcome {
                success: false,
                message: text,
                quarantine,
                dry_run: ctx.dry_run,
            };
        }
    }
    // Independent verification pass over the persisted file.
    publish_progress(Some(UpdateProgress {
        version: version.clone(),
        message: crate::i18n::ui("Güncelleme doğrulanıyor", "Verifying the update").into(),
        downloaded: 0,
        total: None,
    }));
    let verified = (|| -> Result<String> {
        let mut file = fs::File::open(&destination)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        Ok(hex::encode(hasher.finalize()))
    })();
    let actual = match verified {
        Ok(actual) => actual,
        Err(error) => {
            let folder = quarantine_file(ctx.paths, &destination).ok();
            return UpdateOutcome {
                success: false,
                message: crate::i18n::ui_format!(
                    "Güncelleme doğrulaması okunamadı: {error:#}{moved}",
                    "The update verification could not be read: {error:#}{moved}",
                    error = error,
                    moved = folder
                        .as_ref()
                        .map(|f| crate::i18n::ui_format!(
                            "\n\nDosya şu klasöre taşındı:\n{0}",
                            "\n\nThe file was moved to this folder:\n{0}",
                            f.display()
                        ))
                        .unwrap_or_default()
                ),
                quarantine: folder,
                dry_run: ctx.dry_run,
            };
        }
    };
    if !actual.eq_ignore_ascii_case(&manifest.sha256) {
        let folder = quarantine_file(ctx.paths, &destination).ok();
        return UpdateOutcome {
            success: false,
            message: digest_mismatch_message(&manifest.sha256, &actual, folder.as_deref()),
            quarantine: folder,
            dry_run: ctx.dry_run,
        };
    }
    if !staged_setup_is_plausible(&destination) {
        let folder = quarantine_file(ctx.paths, &destination).ok();
        return UpdateOutcome {
            success: false,
            message: crate::i18n::ui(
                "İndirilen kurulum dosyası geçerli bir Windows kurulum paketi değil.",
                "The downloaded installer is not a valid Windows installer package.",
            )
            .into(),
            quarantine: folder,
            dry_run: ctx.dry_run,
        };
    }
    if ctx.dry_run {
        // Prove the NO-DELETE contract in staging: the verified Setup moves to
        // quarantine instead of launching, and the message surfaces the path.
        let folder = quarantine_file(ctx.paths, &destination).ok();
        let message = match &folder {
            Some(f) => crate::i18n::ui_format!("Deneme modu: kurulum başlatılmadı. Doğrulanan Setup dosyası şu klasöre taşındı:\n{0}", "Dry run: the installation was not started. The verified Setup file was moved to this folder:\n{0}", f.display()),
            None => crate::i18n::ui("Deneme modu: kurulum başlatılmadı.", "Dry run: the installation was not started.").into(),
        };
        return UpdateOutcome {
            success: true,
            dry_run: true,
            message,
            quarantine: folder,
        };
    }
    match current_exe_dir() {
        Ok(app_dir) => {
            publish_progress(Some(UpdateProgress {
                version: version.clone(),
                message: crate::i18n::ui(
                    "Kurulum başlatılıyor; uygulama yeniden açılacak",
                    "Starting the installation; the application will reopen",
                )
                .into(),
                downloaded: 0,
                total: None,
            }));
            if let Err(error) = launch_install_helper(ctx.paths, &destination, &app_dir, manifest) {
                let folder = quarantine_file(ctx.paths, &destination).ok();
                return UpdateOutcome {
                    success: false,
                    message: crate::i18n::ui_format!(
                        "Kurulum başlatılamadı: {error:#}{moved}",
                        "The installation could not be started: {error:#}{moved}",
                        error = error,
                        moved = folder
                            .as_ref()
                            .map(|f| crate::i18n::ui_format!(
                                "\n\nKurulum dosyası şu klasöre taşındı:\n{0}",
                                "\n\nThe installer file was moved to this folder:\n{0}",
                                f.display()
                            ))
                            .unwrap_or_default()
                    ),
                    quarantine: folder,
                    dry_run: false,
                };
            }
            UpdateOutcome {
                success: true,
                message: crate::i18n::ui_format!("SSDownload {0} kuruluyor. Uygulama kısa süre içinde yeniden başlayacak. Kurulum dosyası, uygulama yeniden açıldığında karantina klasörüne taşınacak.", "SSDownload {0} is installing. The application will restart shortly. The installer file will move to the quarantine folder when the application reopens.", manifest.version),
                // The helper still reads this file; the next start moves it into
                // the quarantine folder through `reconcile_update_staging`.
                quarantine: None,
                dry_run: false,
            }
        }
        Err(error) => {
            let folder = quarantine_file(ctx.paths, &destination).ok();
            UpdateOutcome {
                success: false,
                message: crate::i18n::ui_format!(
                    "Kurulum yolu belirlenemedi: {error:#}{moved}",
                    "The install path could not be determined: {error:#}{moved}",
                    error = error,
                    moved = folder
                        .as_ref()
                        .map(|f| crate::i18n::ui_format!(
                            "\n\nKurulum dosyası şu klasöre taşındı:\n{0}",
                            "\n\nThe installer file was moved to this folder:\n{0}",
                            f.display()
                        ))
                        .unwrap_or_default()
                ),
                quarantine: folder,
                dry_run: false,
            }
        }
    }
}

static CHECK_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One update check at a time.
///
/// The startup check and the manual "Güncellemeleri denetle..." action both stage
/// `update-staging\SSDownload-<version>-Setup.exe`: run together they download over each
/// other and move one run's verified artifact into quarantine, leaving an empty
/// quarantine folder behind. The flag is released when the owning thread ends, and a
/// failed spawn releases it by dropping the closure.
struct CheckInFlight;

impl CheckInFlight {
    fn acquire() -> Option<Self> {
        // A failed acquisition must not construct a guard: the temporary would be dropped
        // immediately and its `Drop` would clear a flag the running check still owns.
        if CHECK_RUNNING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return None;
        }
        Some(Self)
    }
}

impl Drop for CheckInFlight {
    fn drop(&mut self) {
        CHECK_RUNNING.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Startup entry point: runs the daily-gated check on a background thread and
/// shows the native MB_YESNO dialog when a newer version is available.
/// The newest release a check has seen that is newer than this build, until it is
/// installed. The desktop shows it as a badge in the title and the Yardım menu.
static AVAILABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn available_version() -> Option<String> {
    AVAILABLE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn note_available(version: &str) {
    *AVAILABLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_owned());
}

/// True when the release host answered: a verified feed, or one whose signature failed (a
/// definitive verdict that retrying at once cannot change). Network and HTTP failures leave the
/// daily gate open so the next start tries again.
fn feed_answered(feed: &Result<UpdateManifest, anyhow::Error>) -> bool {
    match feed {
        Ok(_) => true,
        Err(error) => crate::error_codes::code_of(error) == Some(crate::error_codes::UPD_002),
    }
}

/// Records the time of the last answered update check, which gates the next startup check.
fn stamp_update_check(app: &crate::app::App) {
    let mut stamped = app.snapshot().settings;
    stamped.last_update_check = chrono::Local::now().timestamp();
    if let Err(error) = app.update_settings_quiet(stamped) {
        crate::logging::record(crate::logging::Event::warn("update.settings_save").detail(
            crate::i18n::ui_format!(
                "Güncelleme denetim damgası kaydedilemedi: {error:#}",
                "The update check time could not be saved: {error:#}",
                error = error
            ),
        ));
    }
}

/// A stamp from the last day skips the automatic check. One in the future (written while the
/// clock was wrong) must not hold checks back until real time catches up with it.
fn checked_recently(last_check: i64, now: i64) -> bool {
    last_check > 0 && (0..24 * 3600).contains(&(now - last_check))
}

pub fn startup_check(app: crate::app::App) {
    report_install_failure(app.paths());
    let settings = app.snapshot().settings;
    if !settings.update_check_enabled {
        return;
    }
    if checked_recently(settings.last_update_check, chrono::Local::now().timestamp()) {
        return;
    }
    // A manual check already in flight covers this launch; it reports its own outcome.
    let Some(guard) = CheckInFlight::acquire() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let _guard = guard;
            let paths = app.paths().clone();
            let network = app.network_governor();
            let settings = app.snapshot().settings;
            let feed = fetch_feed(&network);
            // An unreachable feed (offline start, flaky network) must not use up the day.
            if feed_answered(&feed) {
                stamp_update_check(&app);
            }
            let CheckOutcome::Available(manifest) = check_with_feed(&settings, feed) else {
                return;
            };
            note_available(&manifest.version);
            if let Some(decision) = crate::gui::offer_update(&manifest) {
                match decision {
                    UpdateDecision::Install => {
                        let ctx = UpdateContext {
                            paths: &paths,
                            network: &network,
                            dry_run: std::env::var_os("SSDOWNLOAD_UPDATE_DRY_RUN")
                                .is_some_and(|v| v == "1"),
                        };
                        let outcome = run_update(&ctx, &manifest);
                        publish_progress(None);
                        crate::gui::report_update_outcome(&outcome);
                        if outcome.success && !outcome.dry_run {
                            // The GUI loop observes quit_requested on its refresh
                            // timer and exits through the normal graceful path; the
                            // detached helper restarts the freshly installed build.
                            std::thread::sleep(Duration::from_millis(300));
                            let _ = app.dispatch(crate::model::Action::Quit);
                        }
                    }
                    UpdateDecision::Skip => {
                        let mut next = app.snapshot().settings;
                        next.skipped_update_version = Some(manifest.version);
                        if let Err(error) = app.update_settings_quiet(next) {
                            crate::logging::record(
                                crate::logging::Event::warn("update.settings_save").detail(
                                    crate::i18n::ui_format!(
                                        "Atlanan sürüm kaydedilemedi: {error:#}",
                                        "The skipped version could not be saved: {error:#}",
                                        error = error
                                    ),
                                ),
                            );
                        }
                    }
                    UpdateDecision::Later => {}
                }
            }
        });
    if let Err(error) = spawned {
        crate::logging::record(crate::logging::Event::warn("update.check_spawn").detail(
            crate::i18n::ui_format!(
                "Güncelleme denetim iş parçacığı başlatılamadı: {error}",
                "The update check thread could not be started: {error}",
                error = error
            ),
        ));
    }
}

pub enum UpdateDecision {
    Install,
    Skip,
    Later,
}

/// "Güncellemeleri şimdi denetle" entry point: explicit, bypasses the daily
/// gate and the skip-version preference, and always reports its outcome.
pub fn check_now(app: crate::app::App) {
    let Some(guard) = CheckInFlight::acquire() else {
        // The in-flight check owns the staged Setup and reports its own result.
        crate::gui::report_update_text(crate::i18n::ui(
            "Güncelleme denetimi zaten sürüyor.",
            "An update check is already running.",
        ));
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("update-check-now".into())
        .spawn(move || {
            let _guard = guard;
            let paths = app.paths().clone();
            let network = app.network_governor();
            let result = fetch_feed(&network);
            if feed_answered(&result) {
                stamp_update_check(&app);
            }
            match result {
                Ok(manifest) => {
                    if is_newer(env!("CARGO_PKG_VERSION"), &manifest.version) {
                        note_available(&manifest.version);
                        if let Some(decision) = crate::gui::offer_update(&manifest) {
                            match decision {
                                UpdateDecision::Install => {
                                    let ctx = UpdateContext {
                                        paths: &paths,
                                        network: &network,
                                        dry_run: std::env::var_os("SSDOWNLOAD_UPDATE_DRY_RUN")
                                            .is_some_and(|v| v == "1"),
                                    };
                                    let outcome = run_update(&ctx, &manifest);
                                    publish_progress(None);
                                    crate::gui::report_update_outcome(&outcome);
                                    if outcome.success && !outcome.dry_run {
                                        std::thread::sleep(Duration::from_millis(300));
                                        // The GUI loop observes quit_requested on its
                                        // refresh timer and destroys the window, which
                                        // ends the process through the normal path.
                                        let _ = app.dispatch(crate::model::Action::Quit);
                                    }
                                }
                                UpdateDecision::Skip => {
                                    let mut next = app.snapshot().settings;
                                    next.skipped_update_version = Some(manifest.version);
                                    if let Err(error) = app.update_settings_quiet(next) {
                                        crate::logging::record(
                                            crate::logging::Event::warn("update.settings_save")
                                                .detail(crate::i18n::ui_format!(
                                                "Atlanan sürüm kaydedilemedi: {error:#}",
                                                "The skipped version could not be saved: {error:#}",
                                                error = error
                                            )),
                                        );
                                    }
                                }
                                UpdateDecision::Later => {}
                            }
                        }
                    } else {
                        crate::gui::report_update_text(&crate::i18n::ui_format!(
                            "SSDownload güncel: sürüm {0} kullanımda.",
                            "SSDownload is up to date; version {0} is in use.",
                            env!("CARGO_PKG_VERSION")
                        ));
                    }
                }
                Err(error) => {
                    crate::gui::report_update_text(&crate::i18n::ui_format!(
                        "Güncelleme denetimi başarısız: {error:#}",
                        "The update check failed: {error:#}",
                        error = error
                    ));
                }
            }
        });
    if let Err(error) = spawned {
        crate::logging::record(crate::logging::Event::warn("update.check_spawn").detail(
            crate::i18n::ui_format!(
                "Güncelleme denetim iş parçacığı başlatılamadı: {error}",
                "The update check thread could not be started: {error}",
                error = error
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `SSDOWNLOAD_UPDATE_FEED` is process-wide; tests that set it must not
    /// interleave or one observes the other's value.
    static FEED_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn semver_comparison() {
        assert!(is_newer("1.4.0", "1.4.1"));
        assert!(is_newer("1.4.0", "2.0.0"));
        assert!(is_newer("1.9.9", "1.10.0"));
        assert!(!is_newer("1.4.1", "1.4.1"));
        assert!(!is_newer("1.4.1", "1.4.0"));
        assert!(!is_newer("1.4.1", "garbage"));
        assert!(!is_newer("garbage", "1.4.2"));
        assert!(
            !is_newer("1.4.0", " 1.5.0 "),
            "a version with surrounding whitespace is not a release name"
        );
        assert!(!is_newer("1.4.0", "1.5.0\n"));
        // The feed publishes stable x.y.z releases only: a prerelease or build suffix is
        // not a release this product ships, so it is neither offered nor assumed newer.
        assert!(!is_newer("1.4.1", "1.4.1-beta"));
        assert!(!is_newer("1.4.0", "1.5.0-beta.2"));
        assert!(!is_newer("1.5.0-beta.1", "1.5.0"));
        assert!(!is_newer("1.4.0", "1.5.0+build.7"));
        assert!(
            !is_newer("1.4", "1.5.0"),
            "a missing component is not a version"
        );
        assert!(
            !is_newer("1.4.0", "1.5"),
            "a missing component is not a version"
        );
        assert!(
            !is_newer("1.3.9.9", "1.5.0"),
            "an extra component is not a version"
        );
        assert!(
            !is_newer("1.4.0", "1.5.0.1"),
            "an extra component is not a version"
        );
        assert!(
            !is_newer("1.4.0", "v1.5.0"),
            "a tag prefix is not a version"
        );
    }

    #[test]
    fn feed_url_override() {
        let _guard = FEED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::set_var(
            "SSDOWNLOAD_UPDATE_FEED",
            "http://127.0.0.1:8123/version.json",
        );
        assert_eq!(feed_url(), "http://127.0.0.1:8123/version.json");
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://127.0.0.1:8123");
        assert_eq!(feed_url(), "http://127.0.0.1:8123/version.json");
        std::env::remove_var("SSDOWNLOAD_UPDATE_FEED");
        assert!(feed_url().ends_with("/version.json"));
    }

    #[test]
    fn feed_signatures_are_verified_against_the_embedded_key() {
        // Vector produced by `openssl pkeyutl -sign -rawin` with the release key.
        let document = br#"{"version":"1.4.2","url":"https://example.test/SSDownload-1.4.2-Setup.exe","sha256":"0000000000000000000000000000000000000000000000000000000000000000","notes":"test"}"#;
        let signature = b"MIXsSUMw5TW2yDA7gQhkc4d00pH0qE64Uuadq+T6FxgwalZkiYCvEWrKMY4cb78+AjkBM/hXt3gO0zHfUeZxAg==";
        verify_feed_signature(document, signature).expect("valid signature");

        let mut tampered = document.to_vec();
        tampered[15] = b'9';
        assert!(
            verify_feed_signature(&tampered, signature).is_err(),
            "a modified document must not verify"
        );
        assert!(
            verify_feed_signature(document, b"not base64").is_err(),
            "a malformed signature document must not verify"
        );
        let wrong = STANDARD.encode([0u8; 64]);
        assert!(
            verify_feed_signature(document, wrong.as_bytes()).is_err(),
            "an unrelated signature must not verify"
        );
    }

    #[test]
    fn staging_feeds_are_recognized_by_their_loopback_override() {
        let _guard = FEED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::remove_var("SSDOWNLOAD_UPDATE_FEED");
        assert!(!staging_feed());
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://127.0.0.1:8123");
        assert!(staging_feed());
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://localhost:8123/feed");
        assert!(staging_feed());
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://[::1]:8123");
        assert!(staging_feed());
        std::env::set_var(
            "SSDOWNLOAD_UPDATE_FEED",
            "https://github.com/isolmaz/SSDownload/releases/latest/download",
        );
        assert!(!staging_feed());
        // A host that merely starts with a loopback literal, or a URL that hides one in
        // its credentials, is not loopback: the signature waiver must not follow it.
        for lookalike in [
            "http://127.0.0.1.example.test:8123",
            "http://127.0.0.1@example.test:8123",
            "http://localhost.example.test:8123",
            "http://127.0.0.1.example.test:8123/version.json",
        ] {
            std::env::set_var("SSDOWNLOAD_UPDATE_FEED", lookalike);
            assert!(!staging_feed(), "{lookalike} must not waive the signature");
        }
        for remote in [
            "http://example.test:8123",
            "ftp://127.0.0.1:8123",
            "http://10.0.0.5:8123",
        ] {
            std::env::set_var("SSDOWNLOAD_UPDATE_FEED", remote);
            assert!(!staging_feed(), "{remote} is not a loopback staging base");
        }
        // The signed production path accepts https Setup URLs only; the unsigned staging
        // path accepts the loopback origin the override named, and nothing else.
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://127.0.0.1:8123");
        assert!(downloadable_url("https://example.test/Setup.exe", false).is_some());
        assert!(downloadable_url("http://127.0.0.1:8123/Setup.exe", false).is_none());
        assert!(downloadable_url(
            "http://127.0.0.1:8123/SSDownload-1.4.19-Setup.exe",
            staging_feed()
        )
        .is_some());
        assert!(downloadable_url("http://127.0.0.1.example.test/Setup.exe", true).is_none());
        assert!(downloadable_url("https://example.test/Setup.exe", true).is_none());
        assert!(downloadable_url("file:///C:/Setup.exe", false).is_none());
        assert!(downloadable_url("https://", false).is_none());
        // The fetched URL is the re-serialized parse, so a spelling the two parsers read
        // differently cannot be validated here and then fetched elsewhere: libcurl reads
        // this backslash as part of the userinfo and would connect to evil.test, while
        // `url` reads it as a path separator and keeps the loopback host.
        assert_eq!(
            downloadable_url(r"http://127.0.0.1:8123\@evil.test/Setup.exe", true).as_deref(),
            Some("http://127.0.0.1:8123/@evil.test/Setup.exe")
        );
        assert_eq!(
            downloadable_url("http://127.0.0.1:8123/Setup.exe", true).as_deref(),
            Some("http://127.0.0.1:8123/Setup.exe")
        );
        // The staging feed document is the same re-serialized override.
        std::env::set_var(
            "SSDOWNLOAD_UPDATE_FEED",
            r"http://127.0.0.1:8123\@evil.test",
        );
        assert_eq!(
            staging_feed_url().as_deref(),
            Some("http://127.0.0.1:8123/@evil.test/version.json")
        );
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://localhost:8123/feed");
        assert_eq!(
            staging_feed_url().as_deref(),
            Some("http://localhost:8123/feed/version.json")
        );
        std::env::remove_var("SSDOWNLOAD_UPDATE_FEED");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("ssdownload-update-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Runs the helper script to completion and waits for the application stand-in it
    /// relaunches (a `.cmd` that writes `reopened.txt`).
    fn run_helper_and_wait_for_reopen(
        root: &Path,
        setup: &Path,
        sha256: &str,
        marker: &Path,
    ) -> bool {
        let target = root.join("ssdownload.cmd");
        let reopened = root.join("reopened.txt");
        fs::write(
            &target,
            format!("@echo off\r\necho reopened> \"{}\"\r\n", reopened.display()),
        )
        .unwrap();
        let script = install_helper_script(2_000_000_000, setup, &target, sha256, marker, "9.9.9");
        let status = helper_command(&script).status().expect("powershell starts");
        assert!(status.success(), "the helper itself must finish cleanly");
        for _ in 0..100 {
            if reopened.is_file() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn a_setup_that_cannot_run_leaves_a_marker_and_the_app_is_reopened() {
        let root = temp_root("helper-missing");
        let marker = root.join(INSTALL_FAILURE_MARKER);
        let missing = root.join("SSDownload-9.9.9-Setup.exe");
        assert!(
            run_helper_and_wait_for_reopen(&root, &missing, &"0".repeat(64), &marker),
            "the existing application must be reopened after a failed installation"
        );
        let written: serde_json::Value =
            serde_json::from_slice(fs::read(&marker).expect("failure marker").as_slice()).unwrap();
        assert_eq!(written["version"], "9.9.9");
        assert!(
            written["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("SSDownload-9.9.9-Setup.exe"),
            "the reason must name the Setup that could not be opened: {written}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn helper_paths_with_typographic_quotes_reach_powershell_unchanged() {
        let root =
            PathBuf::from("C:\\Users\\Ali\u{2019}nin \u{2018}x\u{2019} \u{201A}y\u{201B} 'z'");
        let setup = root.join("SSDownload-9.9.9-Setup.exe");
        let script = install_helper_script(
            1,
            &setup,
            &root.join("ssdownload.exe"),
            &"0".repeat(64),
            &root.join(INSTALL_FAILURE_MARKER),
            "9.9.9",
        );
        let encode = |text: &str| {
            let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            STANDARD.encode(bytes)
        };
        // PowerShell's own parser decides: no syntax error, and the Setup path is one literal.
        let check = format!(
            "$text=[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{script}')); \
             $expected=[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{setup}')); \
             $tokens=$null; $errors=$null; \
             $ast=[System.Management.Automation.Language.Parser]::ParseInput($text,[ref]$tokens,[ref]$errors); \
             if ($errors.Count -gt 0) {{ exit 2 }}; \
             $values=$ast.FindAll({{ $args[0] -is [System.Management.Automation.Language.StringConstantExpressionAst] }}, $true) | ForEach-Object {{ $_.Value }}; \
             if ($values -cnotcontains $expected) {{ exit 3 }}; \
             exit 0",
            script = encode(&script),
            setup = encode(&setup.display().to_string()),
        );
        let status = helper_command(&check).status().expect("powershell starts");
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    fn a_future_check_stamp_does_not_hold_back_the_daily_check() {
        let now = 1_800_000_000;
        assert!(checked_recently(now - 3600, now));
        assert!(!checked_recently(now - 25 * 3600, now));
        assert!(!checked_recently(0, now), "a profile that never checked");
        assert!(
            !checked_recently(now + 30 * 24 * 3600, now),
            "a stamp written by a clock that ran ahead"
        );
    }

    #[test]
    fn a_setup_that_exits_with_an_error_is_reported_with_its_exit_code() {
        let root = temp_root("helper-exit");
        let marker = root.join(INSTALL_FAILURE_MARKER);
        // `whoami /S` is an invalid invocation and exits non-zero, like a failed silent Setup.
        let setup = root.join("SSDownload-9.9.9-Setup.exe");
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        fs::copy(system.join(r"System32\whoami.exe"), &setup).unwrap();
        let sha256 = hex::encode(Sha256::digest(fs::read(&setup).unwrap()));
        assert!(run_helper_and_wait_for_reopen(
            &root, &setup, &sha256, &marker
        ));
        let written: serde_json::Value =
            serde_json::from_slice(fs::read(&marker).expect("failure marker").as_slice()).unwrap();
        let reason = written["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("Setup exited with code"), "{written}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_helper_marker_is_reported_once_and_moved_into_quarantine() {
        let root = temp_root("marker");
        let paths = AppPaths::new(root.clone()).unwrap();
        assert!(
            take_install_failure(&paths).is_none(),
            "no marker, no report"
        );
        let marker = root.join(INSTALL_FAILURE_MARKER);
        fs::write(
            &marker,
            "\u{feff}{\"version\":\"9.9.9\",\"reason\":\"Setup exited with code 2\"}",
        )
        .unwrap();
        let message = take_install_failure(&paths).expect("the marker is reported");
        assert!(
            message.contains("9.9.9") && message.contains("Setup exited with code 2"),
            "{message}"
        );
        assert!(!marker.exists(), "the marker moves out of the profile root");
        let quarantined = fs::read_dir(root.join("update-quarantine"))
            .unwrap()
            .flatten()
            .any(|folder| folder.path().join(INSTALL_FAILURE_MARKER).is_file());
        assert!(
            quarantined,
            "the marker is kept in quarantine, never deleted"
        );
        assert!(take_install_failure(&paths).is_none(), "reported only once");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_the_registered_install_folder_counts_as_the_installed_copy() {
        let installed = temp_root("installed");
        let portable = temp_root("portable");
        let respelled = PathBuf::from(format!("{}\\", installed.to_string_lossy().to_uppercase()));
        assert!(is_install_folder(&installed, Some(&installed)));
        assert!(
            is_install_folder(&installed, Some(&respelled)),
            "case and a trailing separator do not make a different folder"
        );
        assert!(!is_install_folder(&portable, Some(&installed)));
        assert!(
            !is_install_folder(&portable, None),
            "without an installer registration every copy is portable"
        );
        fs::remove_dir_all(installed).ok();
        fs::remove_dir_all(portable).ok();
    }

    #[test]
    fn a_copied_original_is_renamed_so_it_is_not_staged_again() {
        let root = temp_root("retire");
        let paths = AppPaths::new(root.clone()).unwrap();
        let staging = paths.base_dir.join("update-staging");
        fs::create_dir_all(&staging).unwrap();
        let setup = staging.join("SSDownload-9.9.9-Setup.exe");
        fs::write(&setup, b"staged installer").unwrap();
        retire_copied_original(&setup);
        assert!(
            !setup.exists(),
            "the original no longer carries a Setup name"
        );
        assert!(
            staging.join("SSDownload-9.9.9-Setup.exe.moved").is_file(),
            "the original is kept under a .moved name, not deleted"
        );
        assert!(
            reconcile_update_staging(&paths).unwrap().is_none(),
            "a .moved file is not a staged Setup, so nothing is copied again"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_daily_gate_only_closes_when_the_feed_answered() {
        let manifest = UpdateManifest {
            version: "1.0.0".into(),
            url: "https://example.test/Setup.exe".into(),
            sha256: "0".repeat(64),
            notes: String::new(),
        };
        assert!(feed_answered(&Ok(manifest)));
        assert!(feed_answered(&Err(crate::error_codes::coded_default(
            crate::error_codes::UPD_002
        ))));
        assert!(
            !feed_answered(&Err(anyhow!("connection refused"))),
            "an unreachable feed must not use up the day"
        );
        assert!(!feed_answered(&Err(crate::error_codes::coded_default(
            crate::error_codes::UPD_001
        ))));
    }

    /// Serves `body` once to the first request on a loopback port and returns its base URL.
    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        (format!("http://{address}/version.json"), server)
    }

    #[test]
    fn an_oversized_feed_document_is_reported_as_invalid_not_unreachable() {
        let (url, server) = serve_once(vec![b'x'; 4096]);
        let governor = NetworkGovernor::new(4);
        let error = fetch_document(&governor, &url, 1024, true)
            .expect_err("a document over the limit must be refused");
        let text = format!("{error:#}");
        assert!(
            text.contains(crate::error_codes::UPD_001.0),
            "an oversized feed must carry its registry code, got: {text}"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_downloaded_setup_with_another_digest_is_refused_with_its_registry_code() {
        let (url, server) = serve_once(b"MZ this is not the signed installer".to_vec());
        let root = std::env::temp_dir().join(format!("ssdownload-digest-{}", uuid::Uuid::new_v4()));
        let manifest = UpdateManifest {
            version: "9.9.9".into(),
            url,
            sha256: "0".repeat(64),
            notes: String::new(),
        };
        let governor = NetworkGovernor::new(4);
        let error = download_setup(
            &manifest,
            &root.join("Setup.exe"),
            &governor,
            &mut |_, _| {},
            true,
        )
        .expect_err("a digest that differs from the signed feed is refused");
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::UPD_003),
            "{error:#}"
        );
        server.join().unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_digest_mismatch_dialog_carries_the_registry_code_and_both_digests() {
        let expected = "a".repeat(64);
        let actual = "b".repeat(64);
        let folder = Path::new(r"C:\profile\update-quarantine\20260101-000000");
        let text = digest_mismatch_message(&expected, &actual, Some(folder));
        assert_eq!(
            crate::error_codes::from_text(&text),
            Some(crate::error_codes::UPD_003),
            "{text}"
        );
        assert!(text.contains(&expected) && text.contains(&actual), "{text}");
        assert!(text.contains(&folder.display().to_string()), "{text}");
        assert!(
            !digest_mismatch_message(&expected, &actual, None).contains("update-quarantine"),
            "a missing quarantine folder is not named"
        );
    }

    /// Runs a dry-run update whose Setup hashes to something other than the feed's digest and
    /// returns its outcome with the digest the file really has.
    fn refused_update(paths: &AppPaths) -> (UpdateOutcome, String) {
        let _guard = FEED_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let body = b"MZ this is not the signed installer".to_vec();
        let actual = hex::encode(Sha256::digest(&body));
        let (url, server) = serve_once(body);
        std::env::set_var("SSDOWNLOAD_UPDATE_FEED", "http://127.0.0.1:1");
        let manifest = UpdateManifest {
            version: "9.9.9".into(),
            url,
            sha256: "0".repeat(64),
            notes: String::new(),
        };
        let governor = NetworkGovernor::new(4);
        let outcome = run_update(
            &UpdateContext {
                paths,
                network: &governor,
                dry_run: true,
            },
            &manifest,
        );
        std::env::remove_var("SSDOWNLOAD_UPDATE_FEED");
        server.join().unwrap();
        (outcome, actual)
    }

    #[test]
    fn a_refused_download_states_the_mismatch_and_names_the_quarantine_folder_once() {
        let root = temp_root("refused");
        let paths = AppPaths::new(root.clone()).unwrap();
        let (outcome, actual) = refused_update(&paths);

        assert!(!outcome.success);
        let folder = outcome.quarantine.clone().expect("the file was moved");
        assert!(folder.join("SSDownload-9.9.9-Setup.exe").is_file());
        let text = outcome.dialog_text();
        assert_eq!(
            crate::error_codes::from_text(&text),
            Some(crate::error_codes::UPD_003),
            "{text}"
        );
        assert_eq!(text.matches("SSD-UPD-003").count(), 1, "{text}");
        assert!(
            text.contains(&"0".repeat(64)) && text.contains(&actual),
            "{text}"
        );
        assert_eq!(
            text.matches(&folder.display().to_string()).count(),
            1,
            "the folder is named once: {text}"
        );
        // The file is complete and was moved only after the check, so neither a partial
        // download nor an announced move belongs in the statement.
        for stale in [
            "Kısmi",
            "partial",
            "indirilemedi",
            "could not be downloaded",
            "karantinaya taşındı",
            "moved to quarantine",
        ] {
            assert!(!text.contains(stale), "{stale:?} in: {text}");
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_refused_download_that_could_not_be_moved_claims_no_folder() {
        let root = temp_root("refused-unmoved");
        let paths = AppPaths::new(root.clone()).unwrap();
        // A file where the quarantine root belongs makes every move fail.
        fs::write(root.join("update-quarantine"), b"not a folder").unwrap();
        let (outcome, actual) = refused_update(&paths);

        assert!(outcome.quarantine.is_none());
        let text = outcome.dialog_text();
        assert_eq!(
            crate::error_codes::from_text(&text),
            Some(crate::error_codes::UPD_003),
            "{text}"
        );
        assert!(
            text.contains(&"0".repeat(64)) && text.contains(&actual),
            "{text}"
        );
        for claim in [
            "taşındı",
            "moved",
            "update-quarantine",
            "Karantina klasörü",
            "Quarantine folder",
        ] {
            assert!(!text.contains(claim), "{claim:?} in: {text}");
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_refused_update_check_does_not_release_the_running_one() {
        let owner = CheckInFlight::acquire().expect("the first check acquires the guard");
        // Two refused attempts: neither may construct a guard, because the temporary would be
        // dropped at once and clear the flag while `owner` is still running.
        assert!(
            CheckInFlight::acquire().is_none(),
            "a second check is refused"
        );
        assert!(
            CheckInFlight::acquire().is_none(),
            "a third check is refused"
        );
        drop(owner);
        assert!(
            CheckInFlight::acquire().is_some(),
            "the next check starts once the owner finishes"
        );
    }

    #[test]
    fn staged_setup_check_accepts_only_plausible_windows_images() {
        let root =
            std::env::temp_dir().join(format!("ssdownload-update-image-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let setup = root.join("SSDownload-1.4.2-Setup.exe");
        let mut image = vec![0u8; MIN_SETUP_BYTES as usize + 1024];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        image[0x40..0x44].copy_from_slice(b"PE\0\0");
        fs::write(&setup, &image).unwrap();
        assert!(staged_setup_is_plausible(&setup));

        image[..2].copy_from_slice(b"<!");
        fs::write(&setup, &image).unwrap();
        assert!(
            !staged_setup_is_plausible(&setup),
            "PE imzası olmayan veri reddedilmeli"
        );

        fs::write(&setup, b"short").unwrap();
        assert!(
            !staged_setup_is_plausible(&setup),
            "aşırı küçük dosya reddedilmeli"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_setups_move_into_one_quarantine_folder_and_foreign_files_stay() {
        let root = std::env::temp_dir().join(format!(
            "ssdownload-update-staging-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = AppPaths::new(root.clone()).unwrap();
        let staging = paths.base_dir.join("update-staging");
        fs::create_dir_all(&staging).unwrap();
        let setup = staging.join("SSDownload-1.4.2-Setup.exe");
        fs::write(&setup, b"staged installer").unwrap();
        let foreign = staging.join("notlar.txt");
        fs::write(&foreign, b"kullanici dosyasi").unwrap();

        let folder = reconcile_update_staging(&paths)
            .unwrap()
            .expect("karantina");
        assert_eq!(folder.file_name().unwrap().to_str().unwrap().len(), 15);
        assert!(folder.join("SSDownload-1.4.2-Setup.exe").is_file());
        assert!(!setup.exists());
        assert!(foreign.exists(), "yabancı dosya taşınmamalı");
        assert!(
            reconcile_update_staging(&paths).unwrap().is_none(),
            "ikinci açılışta taşınacak dosya kalmamalı"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
