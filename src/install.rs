use crate::{bridge, i18n::ui, paths::AppPaths};
use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "SSDownload";
const SETUP_FILE: &str = "browser-setup.html";

pub fn register_current(paths: &AppPaths) -> Result<()> {
    let executable = std::env::current_exe().context(ui(
        "Çalıştırılabilir dosya yolu alınamadı",
        "The executable path could not be read",
    ))?;
    if !executable.is_file() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Çalıştırılabilir dosya bulunamadı: {0}",
                "The executable was not found: {0}",
                executable.display()
            )
        );
    }
    bridge::register(&executable, paths).context(ui(
        "Tarayıcı native messaging kaydı yapılamadı",
        "The browser Native Messaging registration could not be made",
    ))?;
    crate::extension::register_external().context(ui(
        "Tarayıcı eklentisi Chrome/Edge için kaydedilemedi",
        "The browser extension could not be registered for Chrome/Edge",
    ))?;

    // The installer and portable archive place the unpacked extensions beside the executable.
    // In a source checkout browser_root also finds the repository directory. Generating this
    // page here keeps every displayed path tied to the executable that was actually registered.
    if let Some(root) = browser_root(&executable) {
        write_browser_setup(&root, paths).context(ui(
            "Tarayıcı kurulum sayfası hazırlanamadı",
            "The browser setup page could not be prepared",
        ))?;
    }
    Ok(())
}

pub fn unregister_current(paths: &AppPaths) -> Result<()> {
    bridge::unregister(&std::env::current_exe()?, paths).context(ui(
        "Tarayıcı native messaging kaydı kaldırılamadı",
        "The browser Native Messaging registration could not be removed",
    ))?;
    // Another SSDownload copy may still own the browser connection; the
    // extension stays installed for it.
    if !bridge::host_registered() {
        crate::extension::unregister_external().context(ui(
            "Tarayıcı eklentisi kaydı kaldırılamadı",
            "The browser extension registration could not be removed",
        ))?;
    }
    Ok(())
}

fn startup_name(paths: &AppPaths) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(
        paths.base_dir.to_string_lossy().to_lowercase().as_bytes(),
    ));
    format!("SSDownload-{}", &digest[..16])
}
fn startup_command(paths: &AppPaths) -> Result<String> {
    fn quote(path: &Path) -> String {
        let value = path.to_string_lossy();
        let trailing = value.chars().rev().take_while(|c| *c == '\\').count();
        format!("\"{}{}\"", value, "\\".repeat(trailing))
    }
    Ok(format!(
        "{} --background --data-dir {}",
        quote(&std::env::current_exe()?),
        quote(&paths.base_dir)
    ))
}
pub fn autostart_enabled(paths: &AppPaths) -> Result<bool> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey(RUN_KEY) else {
        return Ok(false);
    };
    let command: String = key.get_value(startup_name(paths)).unwrap_or_default();
    Ok(command.eq_ignore_ascii_case(&startup_command(paths)?))
}
pub fn set_autostart(paths: &AppPaths, enabled: bool) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    let name = startup_name(paths);
    let expected = startup_command(paths)?;
    if enabled {
        key.set_value(&name, &expected)?;
    } else {
        let current: String = key.get_value(&name).unwrap_or_default();
        if current.eq_ignore_ascii_case(&expected) {
            key.delete_value(&name)?;
        }
    }
    // Migrate only this executable's legacy startup entry.
    let legacy: String = key.get_value(RUN_VALUE).unwrap_or_default();
    if legacy.eq_ignore_ascii_case(&format!(
        "\"{}\" --background",
        std::env::current_exe()?.display()
    )) {
        key.delete_value(RUN_VALUE)?;
    }
    Ok(())
}

pub fn open_browser_setup(paths: &AppPaths) -> Result<()> {
    let executable = std::env::current_exe().context(ui(
        "Çalıştırılabilir dosya yolu alınamadı",
        "The executable path could not be read",
    ))?;
    let root = browser_root(&executable).context(ui(
        "Tarayıcı eklentisi dosyaları bulunamadı. Kurulumu onarın veya taşınabilir paketi yeniden açın.",
        "The browser extension files were not found. Repair the installation or unpack the portable package again.",
    ))?;
    bridge::register(&executable, paths).context(ui(
        "Tarayıcı bağlantısı kaydedilemedi",
        "The browser connection could not be registered",
    ))?;
    crate::extension::register_external().context(ui(
        "Tarayıcı eklentisi Chrome/Edge için kaydedilemedi",
        "The browser extension could not be registered for Chrome/Edge",
    ))?;
    let page = write_browser_setup(&root, paths).context(ui(
        "Tarayıcı kurulum sayfası hazırlanamadı",
        "The browser setup page could not be prepared",
    ))?;
    shell_open(&page).context(ui(
        "Tarayıcı kurulum sayfası açılamadı",
        "The browser setup page could not be opened",
    ))
}

/// Applies Mark-of-the-Web provenance to a completed download.
///
/// The alternate data stream is the interoperable representation used by Windows Attachment
/// Manager and browsers. It deliberately marks the file only; this function never opens or
/// executes the downloaded content. Playlist directories are marked file by file.
pub fn mark_download(path: &Path, source: &str) -> Result<()> {
    let source = source.trim();
    if source.is_empty() {
        bail!(
            "{}",
            ui(
                "İndirme kaynak adresi boş olamaz",
                "The download source address cannot be empty"
            )
        );
    }
    if source.len() > 16 * 1024 || source.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        bail!(
            "{}",
            ui(
                "İndirme kaynak adresi güvenli bir Zone.Identifier değeri değil",
                "The download source address is not a safe Zone.Identifier value"
            )
        );
    }
    let mut parsed = url::Url::parse(source).context(ui(
        "İndirme kaynak adresi geçersiz",
        "The download source address is invalid",
    ))?;
    if !matches!(parsed.scheme(), "http" | "https" | "ftp" | "ftps") {
        bail!(
            "{}",
            ui(
                "İndirme kaynağı desteklenen bir İnternet adresi değil",
                "The download source is not a supported Internet address"
            )
        );
    }
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_path("/");
    parsed.set_query(None);
    parsed.set_fragment(None);
    let body = format!(
        "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={}\r\n",
        parsed.as_str()
    );
    mark_download_path(path, body.as_bytes())
}

fn mark_download_path(path: &Path, body: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        crate::i18n::ui_format!(
            "İşaretlenecek indirme bulunamadı: {0}",
            "The download to mark was not found: {0}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "İndirme işareti sembolik bağlantıya uygulanamaz: {0}",
                "The download mark cannot be applied to a symbolic link: {0}",
                path.display()
            )
        );
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).context(ui(
            "Playlist klasörü okunamadı",
            "The playlist folder could not be read",
        ))? {
            mark_download_path(&entry?.path(), body)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "İndirme normal bir dosya değil: {0}",
                "The download is not a regular file: {0}",
                path.display()
            )
        );
    }

    use std::os::windows::io::AsRawHandle;
    let file = std::fs::File::open(path).context(ui(
        "İndirilen dosyanın birimi açılamadı",
        "The volume of the downloaded file could not be opened",
    ))?;
    let mut flags = 0;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context(ui(
            "Dosya sistemi yetenekleri okunamadı",
            "The file system capabilities could not be read",
        ));
    }
    // FAT/exFAT cannot store alternate streams; never create a misleading sidecar there.
    if flags & windows_sys::Win32::System::SystemServices::FILE_NAMED_STREAMS == 0 {
        return Ok(());
    }

    let mut stream_name: OsString = path.as_os_str().to_owned();
    stream_name.push(":Zone.Identifier");
    let stream_path = PathBuf::from(stream_name);
    let mut stream = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&stream_path)
        .with_context(|| {
            crate::i18n::ui_format!(
                "Windows indirme kaynağı işareti yazılamadı: {0}",
                "The Windows download source mark could not be written: {0}",
                path.display()
            )
        })?;
    stream.write_all(body).context(ui(
        "Zone.Identifier verisi yazılamadı",
        "The Zone.Identifier data could not be written",
    ))?;
    stream.flush().context(ui(
        "Zone.Identifier verisi kaydedilemedi",
        "The Zone.Identifier data could not be saved",
    ))?;
    Ok(())
}

fn browser_root(executable: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(parent) = executable.parent() {
        candidates.push(parent.join("browser"));
        for ancestor in parent.ancestors().take(4).skip(1) {
            candidates.push(ancestor.join("browser"));
        }
    }
    if let Ok(current) = std::env::current_dir() {
        candidates.push(current.join("browser"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.join("chromium").join("manifest.json").is_file())
}

fn write_browser_setup(browser_root: &Path, paths: &AppPaths) -> Result<PathBuf> {
    let chromium = browser_root.join("chromium");
    let chromium_url = url::Url::from_directory_path(&chromium).map_err(|_| {
        anyhow::anyhow!(
            "{}",
            ui(
                "Chrome eklenti klasörü URL'ye dönüştürülemedi",
                "The Chrome extension folder could not be converted to a URL"
            )
        )
    })?;
    let chromium_path = html_escape(&chromium.display().to_string());
    let chromium_url = html_escape(chromium_url.as_str());

    let store_url = html_escape(crate::extension::WEB_STORE_LISTING_URL);
    let extension_id = crate::extension::CHROME_EXTENSION_ID;
    let lang = crate::i18n::code();
    let dir = if crate::i18n::right_to_left() {
        "rtl"
    } else {
        "ltr"
    };
    let title = ui(
        "SSDownload tarayıcı eklentisi",
        "SSDownload browser extension",
    );
    let intro = ui(
        "SSDownload, eklentiyi Chrome ve Edge'e kendisi ekler. Tarayıcıyı bir sonraki açışınızda çıkan <b>Yeni eklenti eklendi</b> uyarısında <b>Eklentiyi etkinleştir</b>'e tıklayın.",
        "SSDownload adds the extension to Chrome and Edge itself. The next time you open the browser, click <b>Enable extension</b> on the <b>New extension added</b> notice.",
    );
    let store_heading = ui("Uyarıyı görmediyseniz", "Didn't see the notice?");
    let store_body = ui(
        "Tarayıcıyı tamamen kapatıp yeniden açın veya eklentiyi Chrome Web Mağazası'ndan ekleyin. Edge, Chrome Web Mağazası eklentilerini de kurar.",
        "Close the browser completely and reopen it, or add the extension from the Chrome Web Store. Edge installs Chrome Web Store extensions too.",
    );
    let store_button = ui("Chrome Web Mağazası'nda aç", "Open in the Chrome Web Store");
    let manual_heading = ui("Mağaza olmadan yükleme", "Loading without the store");
    let manual_steps = ui(
        "<li>Adres çubuğunda <b>chrome://extensions</b> (Edge: <b>edge://extensions</b>) açın.</li><li><b>Geliştirici modu</b>nu açın.</li><li><b>Paketlenmemiş öğe yükle</b>'yi seçip aşağıdaki klasörü gösterin.</li>",
        "<li>Open <b>chrome://extensions</b> (Edge: <b>edge://extensions</b>) in the address bar.</li><li>Turn on <b>Developer mode</b>.</li><li>Choose <b>Load unpacked</b> and select the folder below.</li>",
    );
    let folder_button = ui("Eklenti klasörünü aç", "Open the extension folder");
    let id_label = ui("Eklenti kimliği", "Extension ID");
    let host_heading = ui("Uygulama bağlantısı", "Connection to the app");
    let host_body = ui(
        "Eklenti yalnızca bu bilgisayardaki SSDownload ile konuşur. Bağlantı kurulamazsa SSDownload'u açıp <b>Araçlar → Tarayıcı kurulumu</b>'nu yeniden çalıştırın veya <code>ssdownload.exe --register-host</code> komutunu kullanın. Eklenti indirilen dosyaları hiçbir zaman çalıştırmaz.",
        "The extension talks only to SSDownload on this computer. If it cannot connect, open SSDownload and run <b>Tools → Browser setup</b> again, or use <code>ssdownload.exe --register-host</code>. The extension never runs downloaded files.",
    );

    let html = format!(
        r#"<!doctype html>
<html lang="{lang}" dir="{dir}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root{{color-scheme:light dark;font-family:"Segoe UI",system-ui,sans-serif;background:#eef4fb;color:#15243b}}
body{{margin:0}}main{{max-width:880px;margin:40px auto;padding:0 22px}}header{{background:linear-gradient(135deg,#1458c4,#17a3a3);color:white;border-radius:18px;padding:30px;box-shadow:0 12px 34px #1234}}
h1{{margin:0 0 8px;font-size:30px}}header p{{margin:0;line-height:1.55}}section{{background:#fff;color:#15243b;margin:20px 0;padding:24px;border-radius:14px;box-shadow:0 5px 18px #17304c18}}h2{{margin-top:0}}ol{{padding-inline-start:24px;line-height:1.65}}pre{{overflow-wrap:anywhere;white-space:pre-wrap;background:#e9f0f8;border:1px solid #ccdaea;padding:12px;border-radius:8px;user-select:all}}a.button{{display:inline-block;background:#1458c4;color:#fff;text-decoration:none;padding:10px 15px;border-radius:8px;font-weight:600}}small{{color:#52657a}}@media(prefers-color-scheme:dark){{:root{{background:#0d1725;color:#e9f1fc}}section{{background:#17263a;color:#e9f1fc}}pre{{background:#0f1d2e;border-color:#334b67}}small{{color:#aec0d5}}}}
</style></head><body><main>
<header><h1>{title}</h1><p>{intro}</p></header>
<section><h2>{store_heading}</h2><p>{store_body}</p><p><a class="button" href="{store_url}">{store_button}</a></p></section>
<section><h2>{manual_heading}</h2><ol>{manual_steps}</ol><pre>{chromium_path}</pre><p><a class="button" href="{chromium_url}">{folder_button}</a></p><small>{id_label}: {extension_id}</small></section>
<section><h2>{host_heading}</h2><p>{host_body}</p></section>
</main></body></html>"#
    );

    let page = paths.base_dir.join(SETUP_FILE);
    let temporary = paths.base_dir.join(format!("{SETUP_FILE}.tmp"));
    std::fs::write(&temporary, html.as_bytes()).context(ui(
        "Geçici kurulum sayfası yazılamadı",
        "The temporary setup page could not be written",
    ))?;
    std::fs::rename(&temporary, &page)
        .or_else(|first_error| {
            if page.exists() {
                std::fs::remove_file(&page)?;
                std::fs::rename(&temporary, &page)
            } else {
                Err(first_error)
            }
        })
        .context(ui(
            "Tarayıcı kurulum sayfası atomik olarak yerleştirilemedi",
            "The browser setup page could not be placed atomically",
        ))?;
    Ok(page)
}

/// Opens the extension's Chrome Web Store listing in the default browser.
pub fn open_web_store() -> Result<()> {
    let operation = wide("open");
    let target = wide(crate::extension::WEB_STORE_LISTING_URL);
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "ShellExecuteW başarısız oldu (kod {0})",
                "ShellExecuteW failed (code {0})",
                result as isize
            )
        );
    }
    Ok(())
}

fn shell_open(path: &Path) -> Result<()> {
    let operation = wide("open");
    let shell = crate::winpath::shell_target(path).map_err(anyhow::Error::msg)?;
    crate::logging::record(
        crate::logging::Event::info("shell.open_page").detail(format!("path={shell}")),
    );
    let target = wide_os(std::ffi::OsStr::new(&shell));
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "ShellExecuteW başarısız oldu (kod {0})",
                "ShellExecuteW failed (code {0})",
                result as isize
            )
        );
    }
    Ok(())
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_page_uses_the_selected_profile_directory_and_chrome_source() {
        let root =
            std::env::temp_dir().join(format!("ssdownload-install-test-{}", uuid::Uuid::new_v4()));
        let chrome_root = root.join("browser");
        std::fs::create_dir_all(chrome_root.join("chromium")).unwrap();
        std::fs::write(chrome_root.join("chromium/manifest.json"), "{}").unwrap();
        let paths = AppPaths::new(root.join("isolated-profile")).unwrap();

        assert_eq!(
            browser_root(&root.join("ssdownload.exe")),
            Some(chrome_root.clone())
        );
        let page = write_browser_setup(&chrome_root, &paths).unwrap();
        assert_eq!(page, paths.base_dir.join(SETUP_FILE));
        let html = std::fs::read_to_string(&page).unwrap();
        assert!(html.contains(&html_escape(
            &chrome_root.join("chromium").display().to_string()
        )));
        assert!(html.contains(crate::extension::WEB_STORE_LISTING_URL));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn html_escaping_covers_markup_delimiters() {
        assert_eq!(html_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
