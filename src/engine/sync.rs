//! Periodic file synchronization and final file verification.

use super::*;

pub(crate) fn verify_file_result(path: &Path, checksum: Option<&str>) -> Result<u64> {
    let metadata = fs::metadata(path).context(crate::i18n::ui(
        "Aktarım dosyası okunamadı",
        "The transfer file could not be read",
    ))?;
    if !metadata.is_file() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Aktarım sonucu normal bir dosya değil",
                "The transfer result is not a regular file"
            )
        );
    }
    let bytes = metadata.len();
    if let Some(expected) = checksum {
        validate_checksum_text(expected)?;
        let expected = expected
            .trim()
            .strip_prefix("sha256:")
            .unwrap_or(expected.trim());
        let actual = digest_file(path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarımı SHA-256 doğrulaması başarısız",
                    "Browser transfer SHA-256 verification failed"
                )
            );
        }
    }
    Ok(bytes)
}

pub(super) fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| {
        crate::i18n::ui_owned!(
            format!("Dosya okunamadı: {}", path.display()),
            format!("The file could not be read: {}", path.display())
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

#[derive(Default)]
pub(super) struct SyncHeaders {
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
}

pub(super) struct TemporarySyncFile(pub(super) Option<PathBuf>);
impl TemporarySyncFile {
    pub(super) fn path(&self) -> &Path {
        self.0.as_deref().expect("temporary synchronization path")
    }
    pub(super) fn published(&mut self) {
        self.0 = None;
    }
}
impl Drop for TemporarySyncFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

pub(super) fn synchronized_version_path(destination: &Path, sha256: &str) -> Result<PathBuf> {
    let stem = destination
        .file_stem()
        .and_then(|value| value.to_str())
        .context(crate::i18n::ui(
            "Senkronizasyon hedef dosya adı geçersiz",
            "The synchronization target file name is invalid",
        ))?;
    let name = match destination.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{stem}.{sha256}.{extension}"),
        None => format!("{stem}.{sha256}"),
    };
    Ok(destination.with_file_name(name))
}

pub(super) fn replace_sync_file(source: &Path, destination: &Path) -> Result<()> {
    let source = crate::winpath::wide_long(source);
    let destination = crate::winpath::wide_long(destination);
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context(crate::i18n::ui(
            "Senkronizasyon dosyası atomik yayımlanamadı",
            "The synchronization file could not be published atomically",
        ));
    }
    Ok(())
}

pub(super) fn perform_synchronization(
    policy: &SyncPolicy,
    network: &NetworkGovernor,
) -> Result<SyncOutcome> {
    crate::validation::validate_url(&policy.url, true)?;
    let destination = &policy.destination;
    let parent = destination.parent().context(crate::i18n::ui(
        "Senkronizasyon hedef klasörü eksik",
        "The synchronization target folder is missing",
    ))?;
    fs::create_dir_all(parent)?;
    if destination.exists() && destination.is_dir() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon hedefi bir dosya olmalıdır",
                "The synchronization target must be a file"
            )
        );
    }
    let temp_path = parent.join(format!(".ssdownload-sync-{}.tmp", Uuid::new_v4().simple()));
    let mut temporary = TemporarySyncFile(Some(temp_path));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary.path())
        .context(crate::i18n::ui(
            "Senkronizasyon geçici dosyası oluşturulamadı",
            "The synchronization temporary file could not be created",
        ))?;
    let sync_control = TransferControl::default();
    let mut easy = network.wildcard_easy(&sync_control)?;
    easy.url(&policy.url)?;
    easy.follow_location(true)?;
    easy.max_redirections(10)?;
    // Redirects may only lead to http(s): libcurl's default would also follow ftp(s).
    // CURLOPT_REDIR_PROTOCOLS is not wrapped by the curl crate.
    let redirect_protocols =
        (curl_sys::CURLPROTO_HTTP | curl_sys::CURLPROTO_HTTPS) as std::os::raw::c_long;
    if unsafe {
        curl_sys::curl_easy_setopt(
            easy.raw(),
            curl_sys::CURLOPT_REDIR_PROTOCOLS,
            redirect_protocols,
        )
    } != curl_sys::CURLE_OK
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "libcurl yönlendirme protokolleri kısıtlanamadı",
                "The libcurl redirect protocols could not be restricted"
            )
        );
    }
    easy.fail_on_error(false)?;
    easy.connect_timeout(Duration::from_secs(30))?;
    easy.low_speed_limit(1)?;
    easy.low_speed_time(Duration::from_secs(60))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    easy.useragent(concat!(
        "SSDownload/",
        env!("CARGO_PKG_VERSION"),
        " synchronization"
    ))?;
    let mut conditional = List::new();
    if let Some(etag) = policy.etag.as_deref() {
        conditional.append(&format!("If-None-Match: {etag}"))?;
    }
    if let Some(last_modified) = policy.last_modified.as_deref() {
        conditional.append(&format!("If-Modified-Since: {last_modified}"))?;
    }
    easy.http_headers(conditional)?;

    let headers = RefCell::new(SyncHeaders::default());
    let current = RefCell::new(Url::parse(&policy.url)?);
    let downgraded = std::cell::Cell::new(false);
    let mut digest = Sha256::new();
    let mut written = 0u64;
    let mut write_error = None;
    let perform = {
        let mut transfer = easy.transfer();
        transfer.header_function(|line| {
            if line.starts_with(b"HTTP/") {
                *headers.borrow_mut() = SyncHeaders::default();
                return true;
            }
            if let Ok(line) = std::str::from_utf8(line) {
                if let Some((name, value)) = line.split_once(':') {
                    let value = value.trim().to_owned();
                    if name.eq_ignore_ascii_case("location") {
                        let mut current = current.borrow_mut();
                        if let Ok(next) = current.join(&value) {
                            if current.scheme() == "https" && next.scheme() != "https" {
                                downgraded.set(true);
                                return false;
                            }
                            *current = next;
                        }
                    }
                    let mut headers = headers.borrow_mut();
                    if name.eq_ignore_ascii_case("etag") {
                        headers.etag = Some(value);
                    } else if name.eq_ignore_ascii_case("last-modified") {
                        headers.last_modified = Some(value);
                    }
                }
            }
            true
        })?;
        transfer.write_function(|chunk| {
            if let Err(error) = file.write_all(chunk) {
                write_error = Some(error);
                return Ok(0);
            }
            digest.update(chunk);
            written = written.saturating_add(chunk.len() as u64);
            Ok(chunk.len())
        })?;
        transfer.perform()
    };
    if downgraded.get() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon kaynağı HTTPS bağlantısını şifresiz bir adrese yönlendirdi; reddedildi",
                "The synchronization source redirected an HTTPS connection to an insecure address; refused"
            )
        );
    }
    if let Some(error) = write_error {
        return Err(error).context(crate::i18n::ui(
            "Senkronizasyon verisi diske yazılamadı",
            "The synchronization data could not be written to disk",
        ));
    }
    if let Err(error) = perform {
        if let Some(network_error) = easy.network_error() {
            bail!(
                "{}: {network_error}",
                crate::i18n::ui(
                    "Senkronizasyon ağ bağlantısı açılamadı",
                    "Could not open the synchronization network connection"
                )
            );
        }
        return Err(error).context(crate::i18n::ui(
            "Senkronizasyon aktarımı tamamlanamadı",
            "The synchronization transfer could not be completed",
        ));
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    let status = easy.response_code()?;
    let headers = headers.into_inner();
    if status == 304 {
        return Ok(SyncOutcome {
            etag: headers.etag,
            last_modified: headers.last_modified,
            content_sha256: policy.content_sha256.clone(),
            changed: false,
        });
    }
    if matches!(status, 404 | 410) {
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!("Senkronizasyon kaynağı silinmiş (HTTP {status}); önceki çıktı korundu"),
                format!("The synchronization source is gone (HTTP {status}); the previous output was kept")
            )
        );
    }
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!("Senkronizasyon sunucusu HTTP {status} döndürdü; önceki çıktı korundu"),
                format!("The synchronization server returned HTTP {status}; the previous output was kept")
            )
        );
    }
    if fs::metadata(temporary.path())?.len() != written {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon geçici dosya boyutu doğrulanamadı",
                "The synchronization temporary file size could not be verified"
            )
        );
    }
    let sha256 = hex::encode(digest.finalize());
    let changed = policy.content_sha256.as_deref() != Some(sha256.as_str());
    if !changed {
        return Ok(SyncOutcome {
            etag: headers.etag,
            last_modified: headers.last_modified,
            content_sha256: Some(sha256),
            changed: false,
        });
    }

    let target = if policy.overwrite || !destination.exists() {
        destination.clone()
    } else {
        synchronized_version_path(destination, &sha256)?
    };
    let _lock = crate::output::OutputLock::acquire(&target)?;
    if target.exists() && !policy.overwrite {
        if !target.is_file() || digest_file(&target)? != sha256 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Deterministik senkronizasyon sürüm hedefi başka içerik taşıyor; dosyalar korundu",
                    "The deterministic synchronization version target carries different content; files were kept"
                )
            );
        }
        return Ok(SyncOutcome {
            etag: headers.etag,
            last_modified: headers.last_modified,
            content_sha256: Some(sha256),
            changed: true,
        });
    }
    if policy.overwrite {
        replace_sync_file(temporary.path(), &target)?;
    } else {
        fs::rename(temporary.path(), &target).context(crate::i18n::ui(
            "Senkronizasyon sürümü güvenle yayımlanamadı",
            "The synchronization version could not be published safely",
        ))?;
    }
    temporary.published();
    Ok(SyncOutcome {
        etag: headers.etag,
        last_modified: headers.last_modified,
        content_sha256: Some(sha256),
        changed: true,
    })
}
