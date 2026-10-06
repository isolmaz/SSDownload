use crate::{bridge, paths::AppPaths};
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
    let executable = std::env::current_exe().context("Çalıştırılabilir dosya yolu alınamadı")?;
    if !executable.is_file() {
        bail!(
            "Çalıştırılabilir dosya bulunamadı: {}",
            executable.display()
        );
    }
    bridge::register(&executable, paths).context("Tarayıcı native messaging kaydı yapılamadı")?;

    // The installer and portable archive place the unpacked extensions beside the executable.
    // In a source checkout browser_root also finds the repository directory. Generating this
    // page here keeps every displayed path tied to the executable that was actually registered.
    if let Some(root) = browser_root(&executable) {
        write_browser_setup(&root, paths).context("Tarayıcı kurulum sayfası hazırlanamadı")?;
    }
    Ok(())
}

pub fn unregister_current(paths: &AppPaths) -> Result<()> {
    bridge::unregister(&std::env::current_exe()?, paths)
        .context("Tarayıcı native messaging kaydı kaldırılamadı")
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
    let executable = std::env::current_exe().context("Çalıştırılabilir dosya yolu alınamadı")?;
    let root = browser_root(&executable).context(
        "Tarayıcı eklentisi dosyaları bulunamadı. Kurulumu onarın veya taşınabilir paketi yeniden açın.",
    )?;
    bridge::register(&executable, paths).context("Tarayıcı bağlantısı kaydedilemedi")?;
    let page =
        write_browser_setup(&root, paths).context("Tarayıcı kurulum sayfası hazırlanamadı")?;
    shell_open(&page).context("Tarayıcı kurulum sayfası açılamadı")
}

/// Applies Mark-of-the-Web provenance to a completed download.
///
/// The alternate data stream is the interoperable representation used by Windows Attachment
/// Manager and browsers. It deliberately marks the file only; this function never opens or
/// executes the downloaded content. Playlist directories are marked file by file.
pub fn mark_download(path: &Path, source: &str) -> Result<()> {
    let source = source.trim();
    if source.is_empty() {
        bail!("İndirme kaynak adresi boş olamaz");
    }
    if source.len() > 16 * 1024 || source.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        bail!("İndirme kaynak adresi güvenli bir Zone.Identifier değeri değil");
    }
    let mut parsed = url::Url::parse(source).context("İndirme kaynak adresi geçersiz")?;
    if !matches!(parsed.scheme(), "http" | "https" | "ftp" | "ftps") {
        bail!("İndirme kaynağı desteklenen bir İnternet adresi değil");
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
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("İşaretlenecek indirme bulunamadı: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "İndirme işareti sembolik bağlantıya uygulanamaz: {}",
            path.display()
        );
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).context("Playlist klasörü okunamadı")? {
            mark_download_path(&entry?.path(), body)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        bail!("İndirme normal bir dosya değil: {}", path.display());
    }

    use std::os::windows::io::AsRawHandle;
    let file = std::fs::File::open(path).context("İndirilen dosyanın birimi açılamadı")?;
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
        return Err(std::io::Error::last_os_error()).context("Dosya sistemi yetenekleri okunamadı");
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
            format!(
                "Windows indirme kaynağı işareti yazılamadı: {}",
                path.display()
            )
        })?;
    stream
        .write_all(body)
        .context("Zone.Identifier verisi yazılamadı")?;
    stream
        .flush()
        .context("Zone.Identifier verisi kaydedilemedi")?;
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
    let chromium_url = url::Url::from_directory_path(&chromium)
        .map_err(|_| anyhow::anyhow!("Chrome eklenti klasörü URL'ye dönüştürülemedi"))?;
    let chromium_path = html_escape(&chromium.display().to_string());
    let chromium_url = html_escape(chromium_url.as_str());

    let html = format!(
        r#"<!doctype html>
<html lang="tr"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>SSDownload Tarayıcı Kurulumu</title>
<style>
:root{{color-scheme:light dark;font-family:"Segoe UI",system-ui,sans-serif;background:#eef4fb;color:#15243b}}
body{{margin:0}}main{{max-width:880px;margin:40px auto;padding:0 22px}}header{{background:linear-gradient(135deg,#1458c4,#17a3a3);color:white;border-radius:18px;padding:30px;box-shadow:0 12px 34px #1234}}
h1{{margin:0 0 8px;font-size:30px}}header p{{margin:0;line-height:1.55}}section{{background:#fff;color:#15243b;margin:20px 0;padding:24px;border-radius:14px;box-shadow:0 5px 18px #17304c18}}h2{{margin-top:0}}ol{{padding-left:24px;line-height:1.65}}code{{display:block;overflow-wrap:anywhere;background:#e9f0f8;border:1px solid #ccdaea;padding:12px;border-radius:8px;user-select:all}}a.button{{display:inline-block;background:#1458c4;color:#fff;text-decoration:none;padding:10px 15px;border-radius:8px;font-weight:600}}small{{color:#52657a}}@media(prefers-color-scheme:dark){{:root{{background:#0d1725;color:#e9f1fc}}section{{background:#17263a;color:#e9f1fc}}code{{background:#0f1d2e;border-color:#334b67}}small{{color:#aec0d5}}}}
</style></head><body><main>
<header><h1>SSDownload Tarayıcı Eklentisi</h1><p>Eklenti mağazada yayımlanmış gibi gösterilmez. Aşağıdaki dosyalar bu SSDownload kopyasıyla birlikte gelen, yerel ve paketlenmemiş eklenti kaynaklarıdır.</p></header>
<section><h2>Chrome</h2><ol><li>Adres çubuğunda <b>chrome://extensions</b> açın.</li><li><b>Geliştirici modu</b> seçeneğini etkinleştirin.</li><li><b>Paketlenmemiş öğe yükle</b> seçeneğini seçip aşağıdaki klasörü gösterin.</li></ol><code>{chromium_path}</code><p><a class="button" href="{chromium_url}">Chrome eklenti klasörünü aç</a></p><small>Sabit eklenti kimliği: hgndggnlfpnflkmnbddmcnfniamckham</small></section>
<section><h2>Native Messaging bağlantısı</h2><p>SSDownload içindeki tarayıcı kaydı, uygulamanın <b>Tarayıcı bağlantısını kaydet</b> işlemiyle veya <code>ssdownload.exe --register-host</code> komutuyla geçerli Windows kullanıcısı için oluşturulur. Eklenti indirilen dosyaları otomatik çalıştırmaz.</p></section>
</main></body></html>"#
    );

    let page = paths.base_dir.join(SETUP_FILE);
    let temporary = paths.base_dir.join(format!("{SETUP_FILE}.tmp"));
    std::fs::write(&temporary, html.as_bytes()).context("Geçici kurulum sayfası yazılamadı")?;
    std::fs::rename(&temporary, &page)
        .or_else(|first_error| {
            if page.exists() {
                std::fs::remove_file(&page)?;
                std::fs::rename(&temporary, &page)
            } else {
                Err(first_error)
            }
        })
        .context("Tarayıcı kurulum sayfası atomik olarak yerleştirilemedi")?;
    Ok(page)
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
        bail!("ShellExecuteW başarısız oldu (kod {})", result as isize);
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
        assert_eq!(html.matches("://extensions").count(), 1);
        assert_eq!(html.matches("<section>").count(), 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn html_escaping_covers_markup_delimiters() {
        assert_eq!(html_escape("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
