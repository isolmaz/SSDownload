use crate::process::ProcessJob;
use crate::{
    model::{ToolInfo, ToolProgress, TransferControl},
    network::{GovernedEasy, NetworkGovernor},
    paths::AppPaths,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Write},
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, LazyLock, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;
use windows_sys::Win32::{
    Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH},
    System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
};
use zip::ZipArchive;

const MANIFEST_FILE: &str = "current.json";
pub(crate) const USER_AGENT: &str = concat!(
    "SSDownload/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/isolmaz/SSDownload)"
);
const MAX_API_BYTES: usize = 32 * 1024 * 1024;
const MAX_ARCHIVE_EXPANDED: u64 = 3 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
/// The BtbN build of this FFmpeg release branch (`ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip`)
/// is installed; a release branch changes in bug fixes only, a master snapshot in anything.
const FFMPEG_BRANCH: (u32, u32) = (9, 0);
/// Deno stays on this major version. A new major version is a deliberate change here.
const DENO_MAJOR: u64 = 2;
/// How many of the newest Deno releases are read: the most GitHub returns in one page. Once the
/// pinned major version is older than this many releases, the install says so.
const DENO_RELEASE_LIST: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallManifest {
    schema: u32,
    install_id: String,
    installed_at: i64,
    components: Vec<ComponentManifest>,
    files: Vec<InstalledFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledFile {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ComponentManifest {
    name: String,
    version: String,
    release_tag: String,
    release_id: u64,
    published_at: String,
    source_url: String,
    upstream_project: String,
    asset_url: String,
    asset_sha256: String,
    license: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    id: u64,
    tag_name: String,
    html_url: String,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Clone)]
struct CachedStatus {
    manifest_stamp: Option<(u64, u64)>,
    executable_stamps: Vec<Option<(u64, u64)>>,
    file_paths: Vec<PathBuf>,
    value: Vec<ToolInfo>,
}

static STATUS_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ENSURE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Returns the installed tool state without starting any executable or making network requests.
/// Age of the active toolchain in days, `None` when nothing is installed. Sites change
/// often and the extractor follows them, so the desktop refreshes an old toolchain.
pub(crate) fn toolchain_age_days(paths: &AppPaths) -> Option<i64> {
    let manifest = read_manifest(&paths.tools_dir.join(MANIFEST_FILE)).ok()?;
    Some((unix_seconds() - manifest.installed_at).max(0) / 86_400)
}

pub fn status(paths: &AppPaths) -> Vec<ToolInfo> {
    let manifest_path = paths.tools_dir.join(MANIFEST_FILE);
    let stamp = file_stamp(&manifest_path);
    let cache = &*STATUS_CACHE;
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.get(&paths.tools_dir) {
            if cached.manifest_stamp == stamp {
                let now: Vec<_> = cached
                    .file_paths
                    .iter()
                    .map(|path| file_stamp(path))
                    .collect();
                if now == cached.executable_stamps {
                    return cached.value.clone();
                }
            }
        }
    } // Guard released before the recompute: the miss path re-enters STATUS_CACHE.

    let (value, file_paths) = match read_manifest(&manifest_path) {
        Ok(manifest) => {
            let valid =
                verified_toolchain(paths, &crate::model::TransferControl::default()).is_ok();
            let mut value = infos_from_manifest(paths, &manifest);
            if !valid {
                for tool in &mut value {
                    tool.installed = false;
                }
            }
            let files = manifest
                .files
                .iter()
                .map(|file| paths.tools_dir.join(&file.path))
                .collect::<Vec<_>>();
            (value, files)
        }
        Err(_) => (empty_status(paths), Vec::new()),
    };
    let executable_stamps = file_paths.iter().map(|path| file_stamp(path)).collect();
    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            paths.tools_dir.clone(),
            CachedStatus {
                manifest_stamp: stamp,
                executable_stamps,
                file_paths,
                value: value.clone(),
            },
        );
    }
    value
}

/// Installs missing tools without a running application, for the installer. The saved proxy
/// and per-host limit apply when they can be read; the profile database is only read, so
/// its repair, any warning about it and first-run onboarding stay with the application.
pub fn install_missing(paths: &AppPaths) -> Result<()> {
    let settings = crate::store::Store::stored_settings(&paths.database).unwrap_or_default();
    // Like the engine: a proxy that cannot be applied leaves the connection direct.
    let _ = crate::proxy::configure(&settings.proxy);
    let network = NetworkGovernor::new(settings.per_host_limit);
    ensure(
        paths,
        false,
        &network,
        &TransferControl::default(),
        &mut |_| {},
    )?;
    Ok(())
}

/// Installs a complete, verified private toolchain. The active manifest is switched only after
/// every download, extraction and executable validation has succeeded.
pub(crate) fn ensure(
    paths: &AppPaths,
    force_update: bool,
    network: &NetworkGovernor,
    control: &TransferControl,
    progress: &mut dyn FnMut(ToolProgress),
) -> Result<Vec<ToolInfo>> {
    let _serial = ENSURE_LOCK.lock().map_err(|_| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Medya aracı kurulum kilidi bozuldu",
                "The media tool installation lock is corrupted"
            )
        )
    })?;
    fs::create_dir_all(&paths.tools_dir).context(crate::i18n::ui(
        "Medya bileşenleri klasörü oluşturulamadı",
        "The media components folder could not be created",
    ))?;

    let _process_lock = crate::output::OutputLock::acquire(&paths.tools_dir.join("update"))?;
    if let Err(error) = cleanup_versions(paths) {
        emit(
            progress,
            crate::i18n::ui("Temizlik", "Cleanup"),
            &crate::i18n::ui_format!(
                "Eski araç dosyaları korundu: {error}",
                "Old tool files kept: {error}",
                error = error
            ),
            0,
            None,
        );
    }
    let existing = status(paths);
    if !force_update && existing.iter().all(|tool| tool.installed) {
        return Ok(existing);
    }

    emit(
        progress,
        crate::i18n::ui("Medya bileşenleri", "Media components"),
        crate::i18n::ui(
            "Güncel sürüm bilgileri alınıyor",
            "Fetching current version information",
        ),
        0,
        None,
    );
    let yt_release: GithubRelease = github_json(
        "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest",
        control,
        network,
    )
    .context(crate::i18n::ui(
        "yt-dlp sürüm bilgisi alınamadı",
        "The yt-dlp version information could not be retrieved",
    ))?;
    let deno_releases: Vec<GithubRelease> = github_json(
        &format!(
            "https://api.github.com/repos/denoland/deno/releases?per_page={DENO_RELEASE_LIST}"
        ),
        control,
        network,
    )
    .context(crate::i18n::ui(
        "Deno sürüm bilgisi alınamadı",
        "The Deno version information could not be retrieved",
    ))?;
    let deno_release = newest_deno_release(deno_releases)?;
    let ffmpeg_release: GithubRelease = github_json(
        "https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/tags/latest",
        control,
        network,
    )
    .context(crate::i18n::ui(
        "FFmpeg sürüm bilgisi alınamadı",
        "The FFmpeg version information could not be retrieved",
    ))?;

    let yt_asset = exact_asset(&yt_release, "yt-dlp.exe")?;
    let yt_sums = exact_asset(&yt_release, "SHA2-256SUMS")?;
    let deno_asset = exact_asset(&deno_release, "deno-x86_64-pc-windows-msvc.zip")?;
    let deno_sums = exact_asset(&deno_release, "deno-x86_64-pc-windows-msvc.zip.sha256sum")?;
    let ffmpeg_asset = choose_ffmpeg_asset(&ffmpeg_release)?;
    if let Some(notice) = ffmpeg_fallback_notice(&ffmpeg_asset.name) {
        crate::logging::record(
            crate::logging::Event::warn("tools.ffmpeg_branch_fallback").detail(notice),
        );
    }
    let ffmpeg_sums = ffmpeg_release
        .assets
        .iter()
        .find(|a| {
            a.name.eq_ignore_ascii_case("checksums.sha256")
                || a.name.eq_ignore_ascii_case("checksums.sha256.txt")
        })
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui(
                    "FFmpeg yayını upstream SHA-256 checksum dosyası içermiyor",
                    "The FFmpeg release does not include an upstream SHA-256 checksum file"
                )
            )
        })?;

    let install_id = format!("{}-{}", unix_seconds(), Uuid::new_v4().simple());
    let staging = paths.tools_dir.join(format!(".staging-{install_id}"));
    let mut cleanup = CleanupDir::new(staging.clone());
    fs::create_dir_all(&staging).context(crate::i18n::ui(
        "Geçici araç kurulum klasörü oluşturulamadı",
        "The temporary tool installation folder could not be created",
    ))?;

    let yt_checksum_text = fetch_text(
        &yt_sums.browser_download_url,
        "yt-dlp checksum",
        control,
        network,
    )?;
    let yt_expected = checksum_for(&yt_checksum_text, &yt_asset.name)?;
    let yt_dir = staging.join("yt-dlp");
    fs::create_dir_all(&yt_dir)?;
    let yt_path = yt_dir.join("yt-dlp.exe");
    download_verified(
        &yt_asset,
        &yt_path,
        &yt_expected,
        "yt-dlp",
        control,
        network,
        progress,
    )?;

    let deno_checksum_text = fetch_text(
        &deno_sums.browser_download_url,
        "Deno checksum",
        control,
        network,
    )?;
    let deno_expected = checksum_for(&deno_checksum_text, &deno_asset.name)?;
    let deno_zip = staging.join("deno.zip");
    download_verified(
        &deno_asset,
        &deno_zip,
        &deno_expected,
        "Deno",
        control,
        network,
        progress,
    )?;
    emit(
        progress,
        "Deno",
        crate::i18n::ui(
            "Doğrulanmış arşiv güvenle açılıyor",
            "Safely extracting the verified archive",
        ),
        deno_asset.size,
        Some(deno_asset.size),
    );
    let deno_dir = staging.join("deno");
    extract_zip_safely(&deno_zip, &deno_dir).context(crate::i18n::ui(
        "Deno arşivi açılamadı",
        "The Deno archive could not be extracted",
    ))?;
    fs::remove_file(&deno_zip).ok();
    let deno_path = find_file_named(&deno_dir, "deno.exe").ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Deno arşivinde deno.exe bulunamadı",
                "deno.exe was not found in the Deno archive"
            )
        )
    })?;

    let ffmpeg_checksum_text = fetch_text(
        &ffmpeg_sums.browser_download_url,
        "FFmpeg checksum",
        control,
        network,
    )?;
    let ffmpeg_expected = checksum_for(&ffmpeg_checksum_text, &ffmpeg_asset.name)?;
    let ffmpeg_zip = staging.join("ffmpeg.zip");
    download_verified(
        &ffmpeg_asset,
        &ffmpeg_zip,
        &ffmpeg_expected,
        "FFmpeg",
        control,
        network,
        progress,
    )?;
    emit(
        progress,
        "FFmpeg",
        crate::i18n::ui(
            "Doğrulanmış LGPL arşivi güvenle açılıyor",
            "Safely extracting the verified LGPL archive",
        ),
        ffmpeg_asset.size,
        Some(ffmpeg_asset.size),
    );
    let ffmpeg_dir = staging.join("ffmpeg");
    extract_zip_safely(&ffmpeg_zip, &ffmpeg_dir).context(crate::i18n::ui(
        "FFmpeg arşivi açılamadı",
        "The FFmpeg archive could not be extracted",
    ))?;
    fs::remove_file(&ffmpeg_zip).ok();
    let ffmpeg_path = find_file_named(&ffmpeg_dir, "ffmpeg.exe").ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "FFmpeg arşivinde ffmpeg.exe bulunamadı",
                "ffmpeg.exe was not found in the FFmpeg archive"
            )
        )
    })?;
    let ffprobe_path = find_file_named(&ffmpeg_dir, "ffprobe.exe").ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "FFmpeg arşivinde ffprobe.exe bulunamadı",
                "ffprobe.exe was not found in the FFmpeg archive"
            )
        )
    })?;

    let files = code_file_manifest(&staging, &PathBuf::from("versions").join(&install_id))?;
    emit(
        progress,
        crate::i18n::ui("Medya bileşenleri", "Media components"),
        crate::i18n::ui(
            "İndirilen çalıştırılabilir dosyalar doğrulanıyor",
            "Verifying the downloaded executable files",
        ),
        0,
        None,
    );
    let yt_version = validate_version(
        &yt_path,
        &["--ignore-config", "--no-plugin-dirs", "--version"],
        "yt-dlp",
    )?;
    let deno_version = validate_version(&deno_path, &["--version"], "Deno")?;
    let ffmpeg_version = validate_version(&ffmpeg_path, &["-version"], "FFmpeg")?;
    let ffprobe_version = validate_version(&ffprobe_path, &["-version"], "ffprobe")?;

    let version_root = paths.tools_dir.join("versions");
    fs::create_dir_all(&version_root)?;
    let final_dir = version_root.join(&install_id);
    fs::rename(&staging, &final_dir).context(crate::i18n::ui(
        "Doğrulanmış araç kurulumu etkinleştirme alanına taşınamadı",
        "The verified tool installation could not be moved to the activation area",
    ))?;
    cleanup.disarm();
    let mut final_cleanup = CleanupDir::new(final_dir.clone());

    let rel = |path: &Path| -> Result<PathBuf> {
        let inner = path.strip_prefix(&staging).context(crate::i18n::ui(
            "Araç yolu kurulum klasörü dışında",
            "The tool path is outside the installation folder",
        ))?;
        Ok(PathBuf::from("versions").join(&install_id).join(inner))
    };
    let manifest = InstallManifest {
        schema: 2,
        files,
        install_id: install_id.clone(),
        installed_at: unix_seconds(),
        components: vec![
            component(
                "yt-dlp",
                yt_version,
                &yt_release,
                &yt_asset,
                yt_expected,
                "The Unlicense; standalone executable includes its upstream runtime dependencies",
                rel(&yt_path)?,
            ),
            component(
                "Deno",
                deno_version,
                &deno_release,
                &deno_asset,
                deno_expected,
                "MIT",
                rel(&deno_path)?,
            ),
            component(
                "FFmpeg",
                ffmpeg_version,
                &ffmpeg_release,
                &ffmpeg_asset,
                ffmpeg_expected.clone(),
                "LGPL-3.0-or-later build selected; see LICENSE.txt retained in the upstream archive",
                rel(&ffmpeg_path)?,
            ),
            component(
                "ffprobe",
                ffprobe_version,
                &ffmpeg_release,
                &ffmpeg_asset,
                ffmpeg_expected,
                "LGPL-3.0-or-later build selected; see LICENSE.txt retained in the upstream archive",
                rel(&ffprobe_path)?,
            ),
        ],
    };

    let inventory = serde_json::json!({"bomFormat":"CycloneDX","specVersion":"1.6","version":1,"metadata":{"properties":[{"name":"ssdownload:inventory-scope","value":"Downloaded release assets; embedded transitive binary dependencies are governed by the upstream archive notices."}]},"components":manifest.components.iter().map(|component|serde_json::json!({"type":"application","name":component.name,"version":component.version,"hashes":[{"alg":"SHA-256","content":component.asset_sha256}],"externalReferences":[{"type":"distribution","url":component.asset_url},{"type":"vcs","url":component.source_url}],"properties":[{"name":"upstream-license-notice","value":component.license}]})).collect::<Vec<_>>()});
    crate::recovery::atomic_write(
        &final_dir.join("sbom.cdx.json"),
        &serde_json::to_vec_pretty(&inventory)?,
    )?;
    write_manifest_file(&final_dir.join("install-manifest.json"), &manifest).context(
        crate::i18n::ui(
            "Sürüm ve lisans provenans kaydı yazılamadı",
            "The version and license provenance record could not be written",
        ),
    )?;
    if let Err(error) = activate_manifest(paths, &manifest) {
        return Err(error).context(crate::i18n::ui(
            "Yeni araç kurulumu etkinleştirilemedi; önceki kurulum korundu",
            "The new tool installation could not be activated; the previous installation was kept",
        ));
    }
    final_cleanup.disarm();
    if let Err(error) = cleanup_versions(paths) {
        emit(
            progress,
            crate::i18n::ui("Temizlik", "Cleanup"),
            &crate::i18n::ui_format!(
                "Eski araç dosyaları korundu: {error}",
                "Old tool files kept: {error}",
                error = error
            ),
            0,
            None,
        );
    }
    invalidate_cache(paths);
    emit(
        progress,
        crate::i18n::ui("Medya bileşenleri", "Media components"),
        crate::i18n::ui("Medya desteği hazır", "Media support is ready"),
        1,
        Some(1),
    );
    Ok(status(paths))
}

/// Keep verified EXEs and DLLs read-locked for the complete subprocess lifetime.
pub(crate) struct VerifiedTools {
    pub yt_dlp: PathBuf,
    pub deno: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    _files: Vec<File>,
}

type ToolchainCache = HashMap<PathBuf, (String, Arc<VerifiedTools>)>;
static TOOLCHAIN_CACHE: LazyLock<Mutex<ToolchainCache>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(crate) fn verified_toolchain(
    paths: &AppPaths,
    control: &crate::model::TransferControl,
) -> Result<Arc<VerifiedTools>> {
    let manifest = read_manifest(&paths.tools_dir.join(MANIFEST_FILE)).map_err(|error| {
        // The cause keeps its raw text in the log (and thus in diagnostics); the
        // user-facing sentence stays neutral and points at the automatic step.
        crate::logging::record(
            crate::logging::Event::warn("tools.missing").detail(format!("{error:#}")),
        );
        crate::error_codes::coded(
            crate::error_codes::TOL_001,
            crate::i18n::ui(
                "Video ve ses desteği henüz hazır değil; uygulama gerekli bileşenleri kendisi indirir.",
                "Video and audio support is not ready yet; the app downloads the required components itself.",
            ),
        )
    })?;
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&manifest)?));
    if let Some((cached_key, cached)) = TOOLCHAIN_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&paths.tools_dir)
    {
        if cached_key == &key {
            return Ok(cached.clone());
        }
    }
    let files = lock_verified_files(paths, &manifest, control).context(
        crate::i18n::ui(
            "Kurulu medya bileşenlerinin bütünlüğü doğrulanamadı; uygulama bunları yeniden indirir",
            "The integrity of the installed media components could not be verified; the app downloads them again",
        ),
    )?;
    let executable = |name: &str| -> Result<PathBuf> {
        let component = manifest
            .components
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                anyhow!(
                    "{}",
                    crate::i18n::ui_format!(
                        "{name} etkin araç manifestinde bulunamadı",
                        "{name} was not found in the active tool manifest",
                        name = name
                    )
                )
            })?;
        Ok(paths.tools_dir.join(&component.path).canonicalize()?)
    };
    let ffmpeg = executable("FFmpeg")?;
    if executable("ffprobe")?.parent() != ffmpeg.parent() {
        crate::bail_code!(crate::error_codes::TOL_002);
    }
    let verified = Arc::new(VerifiedTools {
        yt_dlp: executable("yt-dlp")?,
        deno: executable("Deno")?,
        ffmpeg,
        ffprobe: executable("ffprobe")?,
        _files: files,
    });
    let mut cache = TOOLCHAIN_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 4 {
        cache.clear();
    }
    cache.insert(paths.tools_dir.clone(), (key, verified.clone()));
    Ok(verified)
}

fn lock_verified_files(
    paths: &AppPaths,
    manifest: &InstallManifest,
    control: &crate::model::TransferControl,
) -> Result<Vec<File>> {
    let root = paths
        .tools_dir
        .join("versions")
        .join(&manifest.install_id)
        .canonicalize()?;
    let mut files = Vec::with_capacity(manifest.files.len());
    for recorded in &manifest.files {
        let path = paths.tools_dir.join(&recorded.path).canonicalize()?;
        if !path.starts_with(&root) {
            crate::bail_code!(crate::error_codes::TOL_002);
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ)
            .open(&path)?;
        if !sha256_file_controlled(&mut file, Some(control))?.eq_ignore_ascii_case(&recorded.sha256)
        {
            crate::bail_code!(
                crate::error_codes::TOL_002,
                "{}",
                crate::i18n::ui_format!(
                    "{0} değiştirilmiş veya bozulmuş",
                    "{0} was modified or corrupted",
                    recorded.path.display()
                )
            );
        }
        files.push(file);
    }
    Ok(files)
}

fn sha256_file(file: &mut File) -> Result<String> {
    sha256_file_controlled(file, None)
}
fn sha256_file_controlled(
    file: &mut File,
    control: Option<&crate::model::TransferControl>,
) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if control.is_some_and(|c| c.stop_requested()) {
            bail!(
                "{}",
                crate::i18n::ui("Araç doğrulaması durduruldu", "Tool verification stopped",)
            );
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn code_file_manifest(root: &Path, prefix: &Path) -> Result<Vec<InstalledFile>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("dll")
                })
            {
                files.push(InstalledFile {
                    path: prefix.join(path.strip_prefix(root)?),
                    sha256: sha256_file(&mut File::open(&path)?)?,
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn component(
    name: &str,
    version: String,
    release: &GithubRelease,
    asset: &GithubAsset,
    sha256: String,
    license: &str,
    path: PathBuf,
) -> ComponentManifest {
    ComponentManifest {
        name: name.into(),
        version,
        release_tag: release.tag_name.clone(),
        release_id: release.id,
        published_at: release.published_at.clone().unwrap_or_default(),
        source_url: release.html_url.clone(),
        upstream_project: match name {
            "yt-dlp" => "https://github.com/yt-dlp/yt-dlp",
            "Deno" => "https://github.com/denoland/deno",
            _ => "https://ffmpeg.org/",
        }
        .into(),
        asset_url: asset.browser_download_url.clone(),
        asset_sha256: sha256,
        license: license.into(),
        path,
    }
}

fn github_json<T: serde::de::DeserializeOwned>(
    url: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<T> {
    let bytes = fetch_bytes(url, MAX_API_BYTES, None, control, network)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        let body = String::from_utf8_lossy(&bytes);
        anyhow!(
            "{}",
            crate::i18n::ui_format!(
                "GitHub geçersiz sürüm yanıtı döndürdü: {error}; {0}",
                "GitHub returned an invalid release response: {error}; {0}",
                bounded(&body, 2048),
                error = error
            )
        )
    })
}

fn exact_asset(release: &GithubRelease, name: &str) -> Result<GithubAsset> {
    release
        .assets
        .iter()
        .find(|a| a.name == name)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "{1} yayınında gerekli {0} varlığı bulunamadı",
                    "The required {0} asset was not found in release {1}",
                    name,
                    release.tag_name
                )
            )
        })
}

/// The newest stable release of the pinned Deno major version. The list is in publication order,
/// which also holds the patch releases of older major versions, so the version decides.
fn newest_deno_release(releases: Vec<GithubRelease>) -> Result<GithubRelease> {
    let list_was_full = releases.len() >= DENO_RELEASE_LIST;
    releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| Some((stable_version(&release.tag_name)?, release)))
        .filter(|(version, _)| version.0 == DENO_MAJOR)
        .max_by_key(|(version, _)| *version)
        .map(|(_, release)| release)
        .ok_or_else(|| {
            if list_was_full {
                anyhow!(
                    "{}",
                    crate::i18n::ui_format!(
                        "Deno {0}. sürüm serisinden kararlı bir yayın son {1} Deno yayını arasında yok; uygulamanın bu sürümü çok eski bir Deno sürümüne bağlı",
                        "No stable release of Deno {0}.x is among the newest {1} Deno releases; this version of the application is tied to a Deno version that is too old",
                        DENO_MAJOR,
                        DENO_RELEASE_LIST
                    )
                )
            } else {
                anyhow!(
                    "{}",
                    crate::i18n::ui_format!(
                        "Deno {0}. sürüm serisinde kararlı bir yayın bulunamadı",
                        "No stable release of Deno {0}.x was found",
                        DENO_MAJOR
                    )
                )
            }
        })
}

/// `v2.9.8` as `(2, 9, 8)`; a tag with a suffix such as `v2.10.0-rc.1` is not a stable version.
fn stable_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// The LGPL build of the pinned FFmpeg release branch, the shared one when both are published.
/// While the pinned branch is not published, the newest release branch that is published is
/// taken. The master snapshots in the same release are never used.
fn choose_ffmpeg_asset(release: &GithubRelease) -> Result<GithubAsset> {
    choose_ffmpeg_branch_asset(release, FFMPEG_BRANCH)
}

fn choose_ffmpeg_branch_asset(release: &GithubRelease, pinned: (u32, u32)) -> Result<GithubAsset> {
    let builds = release
        .assets
        .iter()
        .filter_map(|asset| {
            let (branch, shared) = ffmpeg_branch(&asset.name)?;
            Some((branch, shared, asset))
        })
        .collect::<Vec<_>>();
    builds
        .iter()
        .filter(|(branch, ..)| *branch == pinned)
        .max_by_key(|(_, shared, _)| *shared)
        .or_else(|| {
            builds
                .iter()
                .max_by_key(|(branch, shared, _)| (*branch, *shared))
        })
        .map(|(.., asset)| (*asset).clone())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui(
                    "BtbN latest yayınında Windows x64 LGPL FFmpeg sürüm dalı ZIP varlığı bulunamadı",
                    "No Windows x64 LGPL FFmpeg release-branch ZIP asset was found in the BtbN latest release"
                )
            )
        })
}

/// The log text for an asset of another release branch than the pinned one, `None` for the
/// pinned branch.
fn ffmpeg_fallback_notice(asset_name: &str) -> Option<String> {
    (ffmpeg_branch(asset_name).map(|(branch, _)| branch) != Some(FFMPEG_BRANCH)).then(|| {
        format!(
            "FFmpeg {}.{} is not published; installing {asset_name}",
            FFMPEG_BRANCH.0, FFMPEG_BRANCH.1
        )
    })
}

/// The release branch and whether the build is the shared one, for a BtbN asset name such as
/// `ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip`. Master snapshots, GPL builds, other platforms
/// and names whose version differs from their branch give `None`.
fn ffmpeg_branch(name: &str) -> Option<((u32, u32), bool)> {
    let rest = name.strip_prefix("ffmpeg-n")?.strip_suffix(".zip")?;
    let (branch, rest) = rest.split_once("-latest-win64-lgpl")?;
    let (shared, version) = match rest.strip_prefix("-shared-") {
        Some(version) => (true, version),
        None => (false, rest.strip_prefix('-')?),
    };
    if version != branch {
        return None;
    }
    let (major, minor) = branch.split_once('.')?;
    Some(((major.parse().ok()?, minor.parse().ok()?), shared))
}

fn fetch_text(
    url: &str,
    label: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<String> {
    let bytes = fetch_bytes(url, 8 * 1024 * 1024, None, control, network).with_context(|| {
        crate::i18n::ui_format!(
            "{label} indirilemedi",
            "{label} could not be downloaded",
            label = label
        )
    })?;
    String::from_utf8(bytes).with_context(|| {
        crate::i18n::ui_format!("{label} UTF-8 değil", "{label} is not UTF-8", label = label)
    })
}

fn fetch_bytes(
    url: &str,
    limit: usize,
    mut progress: Option<&mut dyn FnMut(u64, Option<u64>)>,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<Vec<u8>> {
    let mut easy = configured_easy(url, control, network)?;
    let mut data = Vec::new();
    let mut too_large = false;
    easy.timeout(Duration::from_secs(5 * 60))?;
    easy.progress(true)?;
    let transfer_result = {
        let mut transfer = easy.transfer();
        transfer.write_function(|chunk| {
            if data.len().saturating_add(chunk.len()) > limit {
                too_large = true;
                return Ok(0);
            }
            data.extend_from_slice(chunk);
            Ok(chunk.len())
        })?;
        transfer.progress_function(|total, downloaded, _, _| {
            if let Some(callback) = progress.as_deref_mut() {
                callback(downloaded.max(0.0) as u64, positive_total(total));
            }
            !control.stop_requested()
        })?;
        transfer.perform()
    };
    if let Err(error) = transfer_result {
        if control.stop_requested() {
            bail!("{}", tools_stopped());
        }
        if too_large {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Sunucu yanıtı izin verilen {limit} bayt sınırını aştı",
                    "The server response exceeded the allowed {limit} byte limit",
                    limit = limit
                )
            );
        }
        return Err(error).context(crate::i18n::ui(
            "HTTPS aktarımı başarısız",
            "The HTTPS transfer failed",
        ));
    }
    let status = easy.response_code()?;
    if !(200..300).contains(&status) {
        let body = String::from_utf8_lossy(&data);
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "GitHub HTTP {status} döndürdü: {0}",
                "GitHub returned HTTP {status}: {0}",
                bounded(&body, 2048),
                status = status
            )
        );
    }
    Ok(data)
}

fn configured_easy(
    url: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<GovernedEasy> {
    if !url.starts_with("https://") {
        crate::bail_code!(crate::error_codes::TOL_002);
    }
    let mut easy = network.wildcard_easy(control)?;
    easy.url(url)?;
    easy.useragent(USER_AGENT)?;
    // libcurl rejects non-HTTPS redirects before opening the redirected connection.
    for option in [
        curl_sys::CURLOPT_PROTOCOLS,
        curl_sys::CURLOPT_REDIR_PROTOCOLS,
    ] {
        let code = unsafe {
            curl_sys::curl_easy_setopt(
                easy.raw(),
                option,
                curl_sys::CURLPROTO_HTTPS as std::os::raw::c_long,
            )
        };
        if code != curl_sys::CURLE_OK {
            bail!(
                "{}",
                crate::i18n::ui(
                    "HTTPS protokol sınırı uygulanamadı",
                    "The HTTPS protocol limit could not be applied",
                )
            )
        }
    }
    easy.follow_location(true)?;
    easy.max_redirections(10)?;
    easy.fail_on_error(false)?;
    easy.connect_timeout(Duration::from_secs(30))?;
    // No total limit: a slow but moving download must be able to finish, a stalled one must not
    // hang. Below 1 KiB/s for a minute aborts the attempt, which is then resumed.
    easy.low_speed_limit(1024)?;
    easy.low_speed_time(Duration::from_secs(60))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    Ok(easy)
}

fn tools_stopped() -> &'static str {
    crate::i18n::ui("Araç kurulumu durduruldu", "Tool installation stopped")
}

const DOWNLOAD_ATTEMPTS: u32 = 6;

/// Downloads a staged file, resuming from the bytes already on disk after a dropped or stalled
/// connection. The digest is always computed over the complete file, so a resumed prefix is
/// never trusted without being verified.
fn download_verified(
    asset: &GithubAsset,
    destination: &Path,
    expected: &str,
    label: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(ToolProgress),
) -> Result<()> {
    let mut attempt = 1;
    loop {
        let error = match download_attempt(
            asset,
            destination,
            expected,
            label,
            control,
            network,
            progress,
        ) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let transport = error.downcast_ref::<curl::Error>();
        let retryable =
            !control.stop_requested() && attempt < DOWNLOAD_ATTEMPTS && transport.is_some();
        if !retryable {
            return Err(error);
        }
        // CURLE_RANGE_ERROR: the server ignored the Range request, so the prefix is useless.
        if transport.is_some_and(|error| error.code() == 33) {
            fs::remove_file(destination).ok();
        }
        emit(
            progress,
            label,
            crate::i18n::ui(
                "Bağlantı kesildi; indirme kaldığı yerden sürdürülüyor",
                "Connection lost; resuming the download",
            ),
            0,
            None,
        );
        for _ in 0..(attempt * 4) {
            if control.stop_requested() {
                return Err(error);
            }
            thread::sleep(Duration::from_millis(500));
        }
        attempt += 1;
    }
}

fn download_attempt(
    asset: &GithubAsset,
    destination: &Path,
    expected: &str,
    label: &str,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(ToolProgress),
) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui("Geçersiz araç hedefi", "Invalid tool destination")
        )
    })?;
    fs::create_dir_all(parent)?;
    let mut hasher = Sha256::new();
    let mut seen = 0u64;
    let existing = fs::metadata(destination).map(|m| m.len()).unwrap_or(0);
    let mut file = if existing > 0 && (asset.size == 0 || existing < asset.size) {
        let mut previous = File::open(destination).with_context(|| {
            crate::i18n::ui_format!(
                "{0} okunamadı",
                "{0} could not be read",
                destination.display()
            )
        })?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = previous.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            seen += count as u64;
        }
        fs::OpenOptions::new()
            .append(true)
            .open(destination)
            .with_context(|| {
                crate::i18n::ui_format!(
                    "{0} açılamadı",
                    "{0} could not be opened",
                    destination.display()
                )
            })?
    } else {
        File::create(destination).with_context(|| {
            crate::i18n::ui_format!(
                "{0} oluşturulamadı",
                "{0} could not be created",
                destination.display()
            )
        })?
    };
    let resumed_from = seen;
    let mut easy = configured_easy(&asset.browser_download_url, control, network)?;
    if resumed_from > 0 {
        easy.resume_from(resumed_from)?;
    }
    let mut io_error = None;
    let limit = if asset.size > 0 {
        asset.size.min(1024 * 1024 * 1024)
    } else {
        1024 * 1024 * 1024
    };
    if asset.size > limit {
        bail!(
            "{}",
            crate::i18n::ui(
                "SSD-TOL-002 Araç arşivi 1 GiB sınırını aşıyor",
                "SSD-TOL-002 The tool archive exceeds the 1 GiB limit",
            )
        )
    }
    let mut oversized = false;
    easy.progress(true)?;
    let transfer_result = {
        let mut transfer = easy.transfer();
        transfer.write_function(|chunk| {
            if seen.saturating_add(chunk.len() as u64) > limit {
                oversized = true;
                return Ok(0);
            }
            if let Err(error) = file.write_all(chunk) {
                io_error = Some(error);
                return Ok(0);
            }
            hasher.update(chunk);
            seen = seen.saturating_add(chunk.len() as u64);
            Ok(chunk.len())
        })?;
        transfer.progress_function(|total, downloaded, _, _| {
            emit(
                progress,
                label,
                crate::i18n::ui("İndiriliyor", "Downloading"),
                resumed_from.saturating_add(downloaded.max(0.0) as u64),
                (asset.size > 0)
                    .then_some(asset.size)
                    .or_else(|| positive_total(total)),
            );
            !control.stop_requested()
        })?;
        transfer.perform()
    };
    if control.stop_requested() {
        bail!("{}", tools_stopped());
    }
    if oversized {
        bail!(
            "{}",
            crate::i18n::ui(
                "Araç indirmesi beklenen boyut sınırını aştı",
                "The tool download exceeded the expected size limit",
            )
        )
    }
    if let Err(error) = transfer_result {
        if let Some(io_error) = io_error.take() {
            return Err(io_error).context(crate::i18n::ui(
                "Araç indirmesi diske yazılamadı",
                "The tool download could not be written to disk",
            ));
        }
        return Err(error).with_context(|| {
            crate::i18n::ui_format!(
                "{label} HTTPS indirmesi başarısız",
                "{label} HTTPS download failed",
                label = label
            )
        });
    }
    let status = easy.response_code()?;
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "{label} indirmesi HTTP {status} döndürdü",
                "{label} download returned HTTP {status}",
                label = label,
                status = status
            )
        );
    }
    file.flush()?;
    file.sync_all()?;
    if asset.size > 0 && seen != asset.size {
        bail!("{}",
            crate::i18n::ui_format!("{label} indirme boyutu beklenenden farklı (beklenen {0}, alınan {seen})", "{label} download size differs from the expected size (expected {0}, received {seen})", asset.size, label = label, seen = seen));
    }
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        fs::remove_file(destination).ok();
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "{label} SHA-256 doğrulaması başarısız (beklenen {expected}, alınan {actual})",
                "{label} SHA-256 verification failed (expected {expected}, received {actual})",
                actual = actual,
                expected = expected,
                label = label
            )
        );
    }
    emit(
        progress,
        label,
        crate::i18n::ui("SHA-256 doğrulandı", "SHA-256 verified"),
        seen,
        Some(seen),
    );
    Ok(())
}

fn checksum_for(text: &str, filename: &str) -> Result<String> {
    let mut powershell_sha256 = false;
    let mut powershell_hash = None;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "Algorithm" => {
                    powershell_sha256 = value.eq_ignore_ascii_case("SHA256");
                    powershell_hash = None;
                }
                "Hash" if powershell_sha256 => powershell_hash = Some(value),
                "Path" => {
                    if powershell_sha256 && value.rsplit(['\\', '/']).next() == Some(filename) {
                        if let Some(hash) = powershell_hash {
                            if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                                return Ok(hash.to_ascii_lowercase());
                            }
                        }
                    }
                    powershell_sha256 = false;
                    powershell_hash = None;
                }
                _ => {}
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        let name = name.trim_start_matches('*').trim_start_matches("./");
        if name == filename && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!(
        "{}",
        crate::i18n::ui_format!(
            "Upstream checksum dosyasında {filename} için geçerli SHA-256 bulunamadı",
            "No valid SHA-256 for {filename} was found in the upstream checksum file",
            filename = filename
        )
    )
}

fn extract_zip_safely(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!(
            "{}",
            crate::i18n::ui(
                "Arşiv çok fazla girdi içeriyor",
                "The archive contains too many entries",
            )
        );
    }
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        expanded = expanded.checked_add(entry.size()).ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui("Arşiv boyutu taştı", "The archive size overflowed")
            )
        })?;
        if expanded > MAX_ARCHIVE_EXPANDED {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Arşivin açılmış boyutu güvenlik sınırını aşıyor",
                    "The archive's expanded size exceeds the security limit",
                )
            );
        }
        let entry_name = entry.name()?.into_owned();
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "Arşiv güvenli olmayan yol içeriyor: {entry_name}",
                    "The archive contains an unsafe path: {entry_name}",
                    entry_name = entry_name
                )
            )
        })?;
        if enclosed.components().any(|c| {
            matches!(
                c,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        }) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Arşiv hedef klasör dışına çıkan yol içeriyor",
                    "The archive contains a path that escapes the target folder",
                )
            );
        }
        for component in enclosed.components() {
            if let Component::Normal(name) = component {
                if !safe_windows_name(name) {
                    bail!(
                        "{}",
                        crate::i18n::ui_format!(
                            "Arşiv güvenli olmayan Windows dosya adı içeriyor: {entry_name}",
                            "The archive contains an unsafe Windows file name: {entry_name}",
                            entry_name = entry_name
                        )
                    );
                }
            }
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                bail!(
                    "{}",
                    crate::i18n::ui_format!(
                        "Arşiv desteklenmeyen bağlantı/özel dosya içeriyor: {entry_name}",
                        "The archive contains an unsupported link or special file: {entry_name}",
                        entry_name = entry_name
                    )
                );
            }
        }
        let target = destination.join(enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&target)?;
        std::io::copy(&mut entry, &mut output)?;
        output.flush()?;
    }
    Ok(())
}

fn safe_windows_name(name: &OsStr) -> bool {
    let value = name.to_string_lossy();
    if value.is_empty()
        || value.ends_with(' ')
        || value.ends_with('.')
        || value
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let base = value
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn find_file_named(root: &Path, filename: &str) -> Option<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(directory).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let ty = entry.file_type().ok()?;
            if ty.is_dir() {
                pending.push(path);
            } else if ty.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(filename)
            {
                return Some(path);
            }
        }
    }
    None
}

fn validate_version(executable: &Path, args: &[&str], name: &str) -> Result<String> {
    let output =
        command_output_timeout(executable, args, Duration::from_secs(20)).with_context(|| {
            crate::i18n::ui_format!(
                "{name} başlatılamadı",
                "{name} could not be started",
                name = name
            )
        })?;
    if !output.status.success() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "{name} sürüm doğrulaması başarısız: {0}",
                "{name} version verification failed: {0}",
                bounded(&String::from_utf8_lossy(&output.stderr), 4096),
                name = name
            )
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "{name} sürüm çıktısı boş",
                    "The {name} version output is empty",
                    name = name
                )
            )
        })?;
    Ok(line.trim().to_string())
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn command_output_timeout(
    executable: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<CapturedOutput> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = command.spawn().map_err(|error| {
        crate::error_codes::coded(
            crate::error_codes::TOL_003,
            crate::i18n::ui_format!(
                "{0} başlatılamadı: {error}",
                "{0} could not be started: {error}",
                executable.display(),
                error = error
            ),
        )
    })?;
    let job = match ProcessJob::assign(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::error_codes::coded(
                crate::error_codes::TOL_003,
                format!("{error:#}"),
            ));
        }
    };
    let stdout = child.stdout.take().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Alt süreç stdout alınamadı",
                "The child process stdout could not be captured"
            )
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Alt süreç stderr alınamadı",
                "The child process stderr could not be captured"
            )
        )
    })?;
    let out_thread = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.take(1024 * 1024).read_to_end(&mut v);
        v
    });
    let err_thread = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.take(1024 * 1024).read_to_end(&mut v);
        v
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            job.terminate();
            let _ = child.wait();
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Alt süreç {0} saniye içinde yanıt vermedi",
                    "The subprocess did not respond within {0} s",
                    timeout.as_secs()
                )
            );
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
    })
}

fn write_manifest_file(path: &Path, manifest: &InstallManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn activate_manifest(paths: &AppPaths, manifest: &InstallManifest) -> Result<()> {
    let target = paths.tools_dir.join(MANIFEST_FILE);
    let temporary = paths
        .tools_dir
        .join(format!(".{MANIFEST_FILE}.{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    {
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    let from = wide_null(&temporary);
    let to = wide_null(&target);
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        let error = std::io::Error::last_os_error();
        fs::remove_file(&temporary).ok();
        return Err(error).context(crate::i18n::ui(
            "Etkin araç manifesti atomik olarak değiştirilemedi",
            "The active tool manifest could not be replaced atomically",
        ));
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<InstallManifest> {
    let bytes = fs::read(path)?;
    let manifest: InstallManifest = serde_json::from_slice(&bytes)?;
    if manifest.schema != 2 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Desteklenmeyen araç manifesti sürümü",
                "Unsupported tool manifest version",
            )
        );
    }
    if !safe_windows_name(OsStr::new(&manifest.install_id))
        || manifest.files.is_empty()
        || manifest.components.len() != 4
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Araç manifesti eksik veya geçersiz",
                "The tool manifest is missing or invalid",
            )
        );
    }
    let prefix = PathBuf::from("versions").join(&manifest.install_id);
    for file in &manifest.files {
        if !file.path.starts_with(&prefix)
            || !file
                .path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Araç dosyası kaydı geçersiz",
                    "The tool file record is invalid",
                )
            );
        }
    }
    for (name, filename) in [
        ("yt-dlp", "yt-dlp.exe"),
        ("Deno", "deno.exe"),
        ("FFmpeg", "ffmpeg.exe"),
        ("ffprobe", "ffprobe.exe"),
    ] {
        let component = manifest
            .components
            .iter()
            .find(|component| component.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                anyhow!(
                    "{}",
                    crate::i18n::ui_format!(
                        "Araç manifestinde {name} eksik",
                        "{name} is missing from the tool manifest",
                        name = name
                    )
                )
            })?;
        if !component
            .path
            .file_name()
            .is_some_and(|value| value.eq_ignore_ascii_case(filename))
            || !manifest
                .files
                .iter()
                .any(|file| file.path == component.path)
        {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "{name} doğrulanmış dosya listesinde bulunamadı",
                    "{name} was not found in the verified file list",
                    name = name
                )
            );
        }
    }
    Ok(manifest)
}

fn infos_from_manifest(paths: &AppPaths, manifest: &InstallManifest) -> Vec<ToolInfo> {
    ["yt-dlp", "FFmpeg", "ffprobe", "Deno"]
        .iter()
        .map(|name| {
            if let Some(component) = manifest
                .components
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
            {
                let relative_safe = component.path.is_relative()
                    && component
                        .path
                        .components()
                        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
                let path = if relative_safe {
                    paths.tools_dir.join(&component.path)
                } else {
                    paths.tools_dir.join("invalid-manifest-path")
                };
                ToolInfo {
                    name: (*name).into(),
                    installed: relative_safe && path.is_file(),
                    version: component.version.clone(),
                    path,
                }
            } else {
                ToolInfo {
                    name: (*name).into(),
                    installed: false,
                    version: String::new(),
                    path: paths.tools_dir.join(name).with_extension("exe"),
                }
            }
        })
        .collect()
}

fn empty_status(paths: &AppPaths) -> Vec<ToolInfo> {
    ["yt-dlp", "FFmpeg", "ffprobe", "Deno"]
        .iter()
        .map(|name| ToolInfo {
            name: (*name).into(),
            installed: false,
            version: String::new(),
            path: paths.tools_dir.join(name).with_extension("exe"),
        })
        .collect()
}

fn invalidate_cache(paths: &AppPaths) {
    STATUS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&paths.tools_dir);
}

fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((
        metadata.len(),
        modified.as_nanos().min(u64::MAX as u128) as u64,
    ))
}

fn emit(
    progress: &mut dyn FnMut(ToolProgress),
    name: &str,
    message: &str,
    downloaded: u64,
    total: Option<u64>,
) {
    progress(ToolProgress {
        name: name.into(),
        message: message.into(),
        downloaded,
        total,
    });
}

fn positive_total(value: f64) -> Option<u64> {
    (value.is_finite() && value > 0.0).then_some(value as u64)
}
fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    if max < '…'.len_utf8() {
        return String::new();
    }
    let keep = max - '…'.len_utf8();
    let mut start = value.len().saturating_sub(keep);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &value[start..])
}
fn wide_null(value: &Path) -> Vec<u16> {
    crate::winpath::wide_long(value)
}

struct CleanupDir {
    path: PathBuf,
    armed: bool,
}
impl CleanupDir {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for CleanupDir {
    fn drop(&mut self) {
        if self.armed {
            fs::remove_dir_all(&self.path).ok();
        }
    }
}

fn cleanup_versions(paths: &AppPaths) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let current = read_manifest(&paths.tools_dir.join(MANIFEST_FILE))
        .ok()
        .map(|m| m.install_id);
    let root = paths.tools_dir.join("versions");
    let mut versions = Vec::new();
    if root.is_dir() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Ok(manifest) = read_manifest(&entry.path().join("install-manifest.json")) {
                if manifest.install_id == id {
                    versions.push((manifest.installed_at, id, entry.path()));
                }
            }
        }
    }
    versions.sort_by_key(|value| std::cmp::Reverse(value.0));
    let mut remove = versions
        .into_iter()
        .skip(2)
        .filter(|(_, id, _)| Some(id) != current.as_ref())
        .map(|(_, _, p)| p)
        .collect::<Vec<_>>();
    for entry in fs::read_dir(&paths.tools_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".staging-")
            && entry.metadata()?.modified()?.elapsed().unwrap_or_default()
                > Duration::from_secs(24 * 3600)
        {
            remove.push(entry.path());
        }
    }
    fn check(path: &Path) -> Result<()> {
        if fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Araç klasörü yönlendirme içeriyor",
                    "The tool folder contains a link",
                )
            )
        }
        if path.is_dir() {
            for e in fs::read_dir(path)? {
                check(&e?.path())?;
            }
        }
        Ok(())
    }
    let base = paths.tools_dir.canonicalize()?;
    for path in remove {
        if check(&path).is_ok() && path.canonicalize()?.starts_with(&base) {
            fn removable(path: &Path, locks: &mut Vec<File>) -> Result<()> {
                if path.is_dir() {
                    for entry in fs::read_dir(path)? {
                        removable(&entry?.path(), locks)?;
                    }
                } else {
                    locks.push(
                        fs::OpenOptions::new()
                            .access_mode(0x00010000)
                            .share_mode(7)
                            .open(path)?,
                    );
                }
                Ok(())
            }
            let mut locks = Vec::new();
            if removable(&path, &mut locks).is_ok() {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn release_from(tag: &str, draft: bool, prerelease: bool, assets: &[String]) -> GithubRelease {
        let assets = assets
            .iter()
            .enumerate()
            .map(|(index, name)| {
                serde_json::json!({
                    "url": format!("https://api.github.com/repos/o/r/releases/assets/{index}"),
                    "id": index,
                    "name": name,
                    "content_type": "application/zip",
                    "state": "uploaded",
                    "size": 1024,
                    "browser_download_url": format!("https://github.com/o/r/releases/download/{tag}/{name}"),
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "url": "https://api.github.com/repos/o/r/releases/7",
            "id": 7,
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "published_at": "2026-10-31T13:20:35Z",
            "html_url": format!("https://github.com/o/r/releases/tag/{tag}"),
            "assets": assets,
        }))
        .unwrap()
    }

    /// The assets of a BtbN `latest` release: the master snapshot and each branch in `branches`,
    /// in the GPL and LGPL, static and shared flavours, next to other platforms.
    fn btbn_assets(branches: &[&str], master: bool) -> Vec<String> {
        let mut names = vec!["checksums.sha256".to_owned()];
        if master {
            for flavour in ["gpl", "gpl-shared", "lgpl", "lgpl-shared"] {
                names.push(format!("ffmpeg-master-latest-win64-{flavour}.zip"));
            }
            names.push("ffmpeg-master-latest-linux64-lgpl-shared.tar.xz".into());
        }
        for branch in branches {
            for license in ["gpl", "lgpl"] {
                names.push(format!(
                    "ffmpeg-n{branch}-latest-win64-{license}-{branch}.zip"
                ));
                names.push(format!(
                    "ffmpeg-n{branch}-latest-win64-{license}-shared-{branch}.zip"
                ));
            }
            names.push(format!(
                "ffmpeg-n{branch}-latest-winarm64-lgpl-shared-{branch}.zip"
            ));
            names.push(format!(
                "ffmpeg-n{branch}-latest-linux64-lgpl-shared-{branch}.tar.xz"
            ));
        }
        names
    }

    fn chosen(names: &[String], pinned: (u32, u32)) -> Result<String> {
        let release = release_from("latest", false, false, names);
        choose_ffmpeg_branch_asset(&release, pinned).map(|asset| asset.name)
    }

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn ffmpeg_comes_from_the_pinned_branch_and_not_from_a_newer_branch_or_master() {
        let names = btbn_assets(&["8.1", "9.0"], true);
        let release = release_from("latest", false, false, &names);
        let asset = choose_ffmpeg_branch_asset(&release, (8, 1)).unwrap();
        assert_eq!(asset.name, "ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip");
        assert_eq!(
            asset.browser_download_url,
            "https://github.com/o/r/releases/download/latest/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip"
        );
        assert_eq!(
            chosen(&names, (9, 0)).unwrap(),
            "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"
        );
    }

    #[test]
    fn the_static_build_of_the_pinned_branch_is_used_when_no_shared_build_is_published() {
        let names = owned(&[
            "ffmpeg-master-latest-win64-lgpl-shared.zip",
            "ffmpeg-n8.1-latest-win64-lgpl-8.1.zip",
            "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip",
        ]);
        assert_eq!(
            chosen(&names, (8, 1)).unwrap(),
            "ffmpeg-n8.1-latest-win64-lgpl-8.1.zip"
        );
    }

    #[test]
    fn a_missing_pinned_branch_gives_way_to_the_newest_release_branch_that_is_published() {
        let names = btbn_assets(&["8.0", "9.0"], true);
        assert_eq!(
            chosen(&names, (8, 1)).unwrap(),
            "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"
        );
        let names = btbn_assets(&["7.1", "8.0"], true);
        assert_eq!(
            chosen(&names, (8, 1)).unwrap(),
            "ffmpeg-n8.0-latest-win64-lgpl-shared-8.0.zip"
        );
        // Branches are ordered by number, not as text.
        let names = btbn_assets(&["9.0", "10.0"], false);
        assert_eq!(
            chosen(&names, (8, 1)).unwrap(),
            "ffmpeg-n10.0-latest-win64-lgpl-shared-10.0.zip"
        );
    }

    #[test]
    fn the_fallback_branch_still_prefers_its_shared_build() {
        let names = owned(&[
            "ffmpeg-n9.0-latest-win64-lgpl-9.0.zip",
            "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip",
            "ffmpeg-n8.0-latest-win64-lgpl-shared-8.0.zip",
        ]);
        assert_eq!(
            chosen(&names, (8, 1)).unwrap(),
            "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"
        );
    }

    #[test]
    fn a_release_with_only_master_snapshots_installs_nothing() {
        assert!(chosen(&btbn_assets(&[], true), (8, 1)).is_err());
        assert!(chosen(&owned(&["checksums.sha256"]), (8, 1)).is_err());
    }

    #[test]
    fn gpl_other_platform_and_inconsistent_names_are_never_chosen() {
        let names = owned(&[
            "ffmpeg-master-latest-win64-lgpl-shared.zip",
            "ffmpeg-n8.1-latest-win64-gpl-8.1.zip",
            "ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip",
            "ffmpeg-n8.1-latest-winarm64-lgpl-shared-8.1.zip",
            "ffmpeg-n8.1-latest-linux64-lgpl-shared-8.1.tar.xz",
            "ffmpeg-n8.1-latest-win64-lgpl-shared-8.0.zip",
            "ffmpeg-n8-latest-win64-lgpl-shared-8.zip",
            "ffmpeg-n8.1.2-latest-win64-lgpl-shared-8.1.2.zip",
        ]);
        assert!(chosen(&names, (8, 1)).is_err());
    }

    #[test]
    fn the_pin_decides_which_branch_the_installer_takes() {
        let pinned = format!("{}.{}", FFMPEG_BRANCH.0, FFMPEG_BRANCH.1);
        let names = btbn_assets(&[&pinned, "99.0", "0.1"], true);
        let asset = choose_ffmpeg_asset(&release_from("latest", false, false, &names)).unwrap();
        assert_eq!(
            asset.name,
            format!("ffmpeg-n{pinned}-latest-win64-lgpl-shared-{pinned}.zip")
        );
    }

    #[test]
    fn only_a_build_of_another_branch_than_the_pin_counts_as_a_fallback() {
        let pinned = format!("{}.{}", FFMPEG_BRANCH.0, FFMPEG_BRANCH.1);
        let older = format!("{}.1", FFMPEG_BRANCH.0 - 1);
        let newer = format!("{}.0", FFMPEG_BRANCH.0 + 1);
        let installed = |branches: &[&str]| {
            let names = btbn_assets(branches, true);
            choose_ffmpeg_asset(&release_from("latest", false, false, &names))
                .unwrap()
                .name
        };

        assert_eq!(ffmpeg_fallback_notice(&installed(&[&pinned])), None);
        assert_eq!(
            ffmpeg_fallback_notice(&installed(&[&older, &pinned, &newer])),
            None
        );
        for branches in [vec![older.as_str(), newer.as_str()], vec![older.as_str()]] {
            let name = installed(&branches);
            let notice = ffmpeg_fallback_notice(&name).expect("a fallback is noted");
            assert!(
                notice.contains(&name) && notice.contains(&pinned),
                "{notice}"
            );
        }
    }

    fn newest_deno(releases: &[(&str, bool, bool)]) -> Result<String> {
        let assets = owned(&[
            "deno-x86_64-pc-windows-msvc.zip",
            "deno-x86_64-pc-windows-msvc.zip.sha256sum",
        ]);
        let releases = releases
            .iter()
            .map(|(tag, draft, prerelease)| release_from(tag, *draft, *prerelease, &assets))
            .collect();
        newest_deno_release(releases).map(|release| release.tag_name)
    }

    #[test]
    fn deno_stays_on_its_major_version_whatever_newer_majors_and_prereleases_exist() {
        let tag = newest_deno(&[
            ("v3.0.1", false, false),
            ("v3.0.0", false, false),
            ("v2.10.0-rc.2", false, true),
            ("v2.9.8", false, false),
            ("v2.9.7", false, false),
            ("v1.46.3", false, false),
        ])
        .unwrap();
        assert_eq!(tag, "v2.9.8");
    }

    #[test]
    fn a_prerelease_flag_excludes_a_release_with_a_plain_tag() {
        let tag = newest_deno(&[("v2.9.9", false, true), ("v2.9.8", false, false)]).unwrap();
        assert_eq!(tag, "v2.9.8");
    }

    #[test]
    fn a_draft_is_never_installed() {
        let tag = newest_deno(&[("v2.9.9", true, false), ("v2.9.8", false, false)]).unwrap();
        assert_eq!(tag, "v2.9.8");
    }

    #[test]
    fn a_suffixed_tag_is_not_a_stable_release_even_when_it_is_not_flagged() {
        let tag = newest_deno(&[("v2.9.9-rc.1", false, false), ("v2.9.8", false, false)]).unwrap();
        assert_eq!(tag, "v2.9.8");
    }

    #[test]
    fn the_highest_version_wins_whatever_the_order_of_the_list() {
        let tag = newest_deno(&[
            ("v2.8.4", false, false),
            ("v2.9.8", false, false),
            ("v2.9.7", false, false),
        ])
        .unwrap();
        assert_eq!(tag, "v2.9.8");
        // Versions compare as numbers: 2.10.0 is newer than 2.9.8.
        let tag = newest_deno(&[("v2.9.8", false, false), ("v2.10.0", false, false)]).unwrap();
        assert_eq!(tag, "v2.10.0");
    }

    #[test]
    fn only_the_pinned_major_version_is_taken() {
        assert_eq!(
            newest_deno(&[("v3.1.0", false, false), ("v2.0.0", false, false)]).unwrap(),
            "v2.0.0"
        );
        assert!(newest_deno(&[
            ("v20.1.0", false, false),
            ("v3.1.0", false, false),
            ("v1.9.9", false, false),
        ])
        .is_err());
        assert!(newest_deno(&[]).is_err());
    }

    #[test]
    fn a_full_list_without_the_pinned_major_version_names_the_cause() {
        let tags: Vec<String> = (0..DENO_RELEASE_LIST)
            .map(|patch| format!("v{}.0.{patch}", DENO_MAJOR + 1))
            .collect();
        let list: Vec<_> = tags
            .iter()
            .map(|tag| (tag.as_str(), false, false))
            .collect();

        let full = format!("{:#}", newest_deno(&list).unwrap_err());
        let short = format!("{:#}", newest_deno(&list[..3]).unwrap_err());

        let limit = DENO_RELEASE_LIST.to_string();
        assert!(full.contains(&limit), "{full}");
        assert!(!short.contains(&limit), "{short}");
        assert_ne!(full, short);
    }

    #[test]
    fn tool_archives_extract_inside_the_destination_and_escaping_entries_are_refused() {
        let root = std::env::temp_dir().join(format!("ssdownload-zip-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();

        let tool = root.join("tool.zip");
        write_zip(&tool, &[("ffmpeg/bin/ffmpeg.exe", b"binary")]);
        let extracted = root.join("extracted");
        extract_zip_safely(&tool, &extracted).unwrap();
        assert_eq!(
            fs::read(extracted.join("ffmpeg/bin/ffmpeg.exe")).unwrap(),
            b"binary"
        );

        let escaping = root.join("escaping.zip");
        write_zip(&escaping, &[("../escaped.txt", b"payload")]);
        assert!(extract_zip_safely(&escaping, &root.join("jail")).is_err());
        assert!(!root.join("escaped.txt").exists());

        fs::remove_dir_all(root).unwrap();
    }
}
