//! Chromium (Chrome-first) extension policy installation for the fixed
//! SSDownload extension.
//!
//! A per-user `ExtensionInstallForcelist` policy entry (`HKCU`) points at the
//! self-hosted GUpdate XML. Chromium-compatible browsers read the same Chrome
//! hive layout; no extra per-browser work is done here.
//!
//! Chrome 137+ refuses to force-install a self-hosted extension on a device it
//! does not consider enterprise-managed: `chrome://policy` shows the entry as
//! `[BLOCKED]`, the extension ID is blocked outright and a manually loaded copy
//! of the same extension is evicted on the next start. The policy is therefore
//! written only on managed devices (domain-joined or Chrome Browser Cloud
//! Management enrolled), where Chrome honours it; every other machine is
//! pointed at the unpacked copy shipped beside the application instead.
//!
//! NO-DELETE contract: removal never deletes files. Superseded artifacts move
//! into `<data-dir>\update-quarantine\<timestamp>\` and the folder path is
//! surfaced to the user.

use anyhow::{Context, Result};

pub const CHROME_EXTENSION_ID: &str = "hgndggnlfpnflkmnbddmcnfniamckham";
/// Every extension identity the native host answers: the self-hosted/unpacked build
/// (fixed key) plus the IDs the Chrome Web Store and Edge Add-ons assign once listed.
pub const ALLOWED_EXTENSION_IDS: &[&str] = &[CHROME_EXTENSION_ID];
/// Must match browser/chromium/manifest.json `update_url`.
pub const UPDATE_XML_URL: &str =
    "https://github.com/isolmaz/SSDownload/releases/latest/download/update.xml";

/// HKCU ExtensionInstallForcelist key (Chromium-compatible readers included).
const FORCELIST_KEY: &str = r"Software\Policies\Google\Chrome\ExtensionInstallForcelist";
/// HKCU ExtensionSettings key: one JSON value per extension ID.
const SETTINGS_KEY: &str = r"Software\Policies\Google\Chrome\ExtensionSettings";
/// Microsoft Edge reads the same policy layout under its own hive.
const EDGE_FORCELIST_KEY: &str = r"Software\Policies\Microsoft\Edge\ExtensionInstallForcelist";
const EDGE_SETTINGS_KEY: &str = r"Software\Policies\Microsoft\Edge\ExtensionSettings";

/// Policy value payload: `<extension-id>;<update-xml-url>`.
fn forcelist_value() -> String {
    format!("{CHROME_EXTENSION_ID};{UPDATE_XML_URL}")
}

/// `ExtensionSettings` entry for our ID. `override_update_url` makes Chrome keep
/// using the policy URL for later updates instead of the one baked into the
/// installed manifest, so moving the feed does not require reinstalling.
fn settings_value() -> String {
    format!(
        r#"{{"installation_mode":"force_installed","update_url":"{UPDATE_XML_URL}","override_update_url":true}}"#
    )
}

fn is_our_forcelist_data(data: &str) -> bool {
    data.split(';').next() == Some(CHROME_EXTENSION_ID)
}

/// True when the Chromium force-list entry exists.
pub fn policies_installed() -> Result<bool> {
    policies_installed_at(FORCELIST_KEY)
}

fn policies_installed_at(forcelist_key: &str) -> Result<bool> {
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey(forcelist_key) else {
        return Ok(false);
    };
    for (value_name, _value_type) in key.enum_values().flatten() {
        if let Ok(data) = key.get_value::<String, _>(&value_name) {
            if is_our_forcelist_data(&data) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub struct ExtensionInstallReport {
    pub installed: bool,
    /// Chrome parses the entry as `[BLOCKED]` on devices it does not manage, so
    /// no policy was written: the extension must be loaded manually instead.
    pub blocked_by_chrome: bool,
    /// The policy hive rejected the write; a UAC-elevated retry can succeed.
    pub needs_elevation: bool,
    pub warnings: Vec<String>,
}

impl ExtensionInstallReport {
    pub fn summary(&self) -> String {
        let mut text = if self.installed {
            String::from(crate::i18n::ui(
                "Chrome eklenti politikası yazıldı.\n",
                "The Chrome extension policy was written.\n",
            ))
        } else if self.blocked_by_chrome {
            crate::i18n::ui_owned!(format!("Chrome, kurumsal olarak yönetilmeyen cihazlarda Web Mağazası dışındaki eklentilerin politikayla otomatik kurulmasına izin vermiyor.\n\nEklentiyi bir kez elle yükleyin:\n{}\n\nOtomatik kurulum için eklentinin Chrome Web Mağazası'nda yayınlanması gerekir.", manual_load_instructions()), format!("Chrome does not allow extensions outside the Chrome Web Store to be installed by policy on devices it does not manage.\n\nLoad the extension manually once:\n{}\n\nFor automatic installation, the extension must be published on the Chrome Web Store.", manual_load_instructions()))
        } else if self.needs_elevation {
            String::from(crate::i18n::ui("Chrome politika anahtarı yönetici izni gerektiriyor; yönetici onayıyla yeniden deneyin.\n", "The Chrome policy key requires administrator rights; retry with administrator approval.\n"))
        } else {
            String::from(crate::i18n::ui(
                "Chrome eklenti politikası yazılamadı.\n",
                "The Chrome extension policy could not be written.\n",
            ))
        };
        for warning in &self.warnings {
            text.push_str(&format!("- {warning}\n"));
        }
        if self.installed {
            text.push_str(crate::i18n::ui(
                "\nDeğişikliğin uygulanması için Chrome tamamen kapatılıp yeniden açılmalıdır.",
                "\nChrome must be fully closed and reopened for the change to take effect.",
            ));
        }
        text
    }
}

/// Folder of the extension copy shipped with the application.
pub fn loadable_extension_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|parent| parent.join("browser").join("chromium"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(r"browser\chromium"))
}

/// Steps a user follows once to load the unpacked copy manually.
pub fn manual_load_instructions() -> String {
    crate::i18n::ui_owned!(
        format!(
            "chrome://extensions → Geliştirici modu'nu açın → Paketlenmemiş öğe yükle → \n{}",
            loadable_extension_dir().display()
        ),
        format!(
            "chrome://extensions → Turn on Developer mode → Load unpacked → \n{}",
            loadable_extension_dir().display()
        )
    )
}

/// True when Chrome accepts a self-hosted force-install policy: the device is
/// domain-joined or enrolled in Chrome Browser Cloud Management.
pub fn device_is_enterprise_managed() -> bool {
    device_is_domain_joined() || browser_cloud_managed()
}

fn device_is_domain_joined() -> bool {
    use windows_sys::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetGetJoinInformation, NetSetupDomainName,
    };
    unsafe {
        let mut name: *mut u16 = std::ptr::null_mut();
        let mut kind = 0i32;
        let status = NetGetJoinInformation(std::ptr::null(), &mut name, &mut kind);
        if !name.is_null() {
            NetApiBufferFree(name as *const _);
        }
        status == 0 && kind == NetSetupDomainName
    }
}

/// Chrome Browser Cloud Management enrollment token (machine scope).
fn browser_cloud_managed() -> bool {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    const KEY: &str = r"SOFTWARE\Policies\Google\Chrome";
    let hklm = winreg::RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(key) = hklm.open_subkey(KEY) else {
        return false;
    };
    [
        "CloudManagementEnrollmentToken",
        "CloudManagementEnrollmentMandatory",
    ]
    .iter()
    .any(|name| key.get_value::<String, _>(name).is_ok())
}

/// Runs the current executable through a UAC prompt with `argument`.
pub fn run_elevated(argument: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe().context(crate::i18n::ui(
        "Uygulama yolu bulunamadı",
        "The application path could not be found",
    ))?;
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .display()
        .to_string()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let arguments: Vec<u16> = argument.encode_utf16().chain(Some(0)).collect();
    crate::logging::record(
        crate::logging::Event::info("shell.elevate")
            .detail(format!("exe={} args={argument}", exe.display())),
    );
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            arguments.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result as isize) <= 32 {
        anyhow::bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!(
                    "Yönetici izniyle çalıştırma reddedildi (kod {})",
                    result as isize
                ),
                format!(
                    "Running as administrator was denied (code {})",
                    result as isize
                ),
            )
        );
    }
    Ok(())
}

/// Writes the per-user force-list policy. Values owned by other software are
/// never touched: we reuse our own value name or take the first free numeric
/// slot.
pub fn install_policies() -> Result<ExtensionInstallReport> {
    if !device_is_enterprise_managed() {
        // Writing here would produce a `[BLOCKED]` entry that also evicts a
        // manually loaded copy of the extension on the next Chrome start.
        return Ok(ExtensionInstallReport {
            installed: false,
            blocked_by_chrome: true,
            needs_elevation: false,
            warnings: Vec::new(),
        });
    }
    let mut report = install_policies_at(FORCELIST_KEY, SETTINGS_KEY)?;
    // Edge follows Chrome: the same managed-device rule, its own policy hive.
    let edge = install_policies_at(EDGE_FORCELIST_KEY, EDGE_SETTINGS_KEY)?;
    report.needs_elevation |= edge.needs_elevation;
    report.warnings.extend(edge.warnings);
    Ok(report)
}

fn install_policies_at(forcelist_key: &str, settings_key: &str) -> Result<ExtensionInstallReport> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let value = forcelist_value();
    let mut report = ExtensionInstallReport {
        installed: false,
        blocked_by_chrome: false,
        needs_elevation: false,
        warnings: Vec::new(),
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _disposition) = match hkcu.create_subkey(forcelist_key) {
        Ok(result) => result,
        Err(error) => {
            // `HKCU\Software\Policies` grants the interactive user read-only
            // access, so the write needs an elevated token.
            let denied = error.kind() == std::io::ErrorKind::PermissionDenied;
            report.needs_elevation = denied;
            if !denied {
                report.warnings.push(crate::i18n::ui_owned!(
                    format!("Politika anahtarı yazılamadı ({error})"),
                    format!("The policy key could not be written ({error})")
                ));
            }
            return Ok(report);
        }
    };
    let value_names = key
        .enum_values()
        .flatten()
        .map(|(value_name, _)| value_name)
        .collect::<Vec<_>>();
    let mut target: Option<String> = None;
    let mut used: Vec<u32> = Vec::new();
    for value_name in &value_names {
        if let Ok(data) = key.get_value::<String, _>(value_name) {
            if is_our_forcelist_data(&data) {
                target = Some(value_name.clone());
                break;
            }
            if let Ok(index) = value_name.parse::<u32>() {
                used.push(index);
            }
        }
    }
    let target = match target {
        Some(name) => name,
        None => {
            let mut index = 1u32;
            while used.contains(&index) {
                index += 1;
            }
            index.to_string()
        }
    };
    key.set_value(&target, &value).context(crate::i18n::ui(
        "Politika değeri yazılamadı",
        "The policy value could not be written",
    ))?;
    report.installed = true;
    // The settings entry keeps later updates on the policy URL; the force-list
    // entry alone still installs the extension.
    if let Err(error) = hkcu
        .create_subkey(settings_key)
        .and_then(|(key, _)| key.set_value(CHROME_EXTENSION_ID, &settings_value()))
    {
        report.needs_elevation |= error.kind() == std::io::ErrorKind::PermissionDenied;
        report.warnings.push(crate::i18n::ui_owned!(format!("Güncelleme adresi politikası yazılamadı ({error}); kurulum yine de tamamlandı"), format!("The update URL policy could not be written ({error}); the installation still completed")));
    }
    Ok(report)
}

/// Removes the SSDownload force-list entry. Registry values are deleted (that
/// is the uninstall path); no files are involved, so the NO-DELETE contract
/// holds.
pub fn remove_policies() -> Result<String> {
    let chrome = remove_policies_at(FORCELIST_KEY, SETTINGS_KEY)?;
    // Edge entries are removed alongside; the Chrome result is the report.
    let _ = remove_policies_at(EDGE_FORCELIST_KEY, EDGE_SETTINGS_KEY);
    Ok(chrome)
}

fn remove_policies_at(forcelist_key: &str, settings_key: &str) -> Result<String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let mut removed = 0usize;
    let removed_settings = hkcu
        .open_subkey_with_flags(settings_key, KEY_SET_VALUE | KEY_QUERY_VALUE)
        .ok()
        .and_then(|key| key.delete_value(CHROME_EXTENSION_ID).ok())
        .is_some();
    let Ok(key) = hkcu.open_subkey_with_flags(forcelist_key, KEY_SET_VALUE | KEY_QUERY_VALUE)
    else {
        return Ok(if removed_settings {
            crate::i18n::ui(
                "Chrome eklenti politikası kaldırıldı.",
                "The Chrome extension policy was removed.",
            )
            .to_string()
        } else if policies_installed_at(forcelist_key).unwrap_or(false) {
            // The entry exists but the hive refuses the write: clean-up needs an
            // elevated token.
            crate::i18n::ui(
                "Chrome politika kaydı yönetici izni olmadan kaldırılamadı.",
                "The Chrome policy entry could not be removed without administrator rights.",
            )
            .to_string()
        } else {
            crate::i18n::ui(
                "Chrome politika kaydı bulunamadı.",
                "The Chrome policy entry was not found.",
            )
            .to_string()
        });
    };
    let value_names = key
        .enum_values()
        .flatten()
        .map(|(value_name, _)| value_name)
        .collect::<Vec<_>>();
    for value_name in value_names {
        let Ok(data) = key.get_value::<String, _>(&value_name) else {
            continue;
        };
        if is_our_forcelist_data(&data) {
            key.delete_value(&value_name).context(crate::i18n::ui(
                "Politika değeri silinemedi",
                "The policy value could not be deleted",
            ))?;
            removed += 1;
        }
    }
    Ok(if removed == 0 && !removed_settings {
        if policies_installed_at(forcelist_key).unwrap_or(false) {
            // The value is visible but the hive refuses the delete: an elevated
            // retry is required to clean up a `[BLOCKED]` entry.
            crate::i18n::ui(
                "Chrome politika kaydı yönetici izni olmadan kaldırılamadı.",
                "The Chrome policy entry could not be removed without administrator rights.",
            )
            .to_string()
        } else {
            crate::i18n::ui(
                "Chrome politika kaydı bulunamadı.",
                "The Chrome policy entry was not found.",
            )
            .to_string()
        }
    } else {
        crate::i18n::ui(
            "Chrome sabit politika kaydı kaldırıldı.",
            "The Chrome force-install policy entry was removed.",
        )
        .to_string()
    })
}

/// True when the hive refuses writes or deletes for the current token.
pub fn policies_removal_needs_elevation() -> bool {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(FORCELIST_KEY, KEY_SET_VALUE) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

/// Startup offer dialog text ("bir kez sor").
pub fn offer_dialog_body() -> String {
    crate::i18n::ui("SSDownload eklentisi Chrome'a otomatik kurulsun mu?\n\n\
     Sabit politika ile kurulur; eklentinin etkin olması için Chrome tamamen \
     kapatılıp yeniden açılmalıdır.",
        "Should the SSDownload extension be installed in Chrome automatically?\n\nIt is installed through a force-installed policy; Chrome must be fully closed and reopened for the extension to take effect.")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips the two per-user policy values under a scratch hive and
    /// checks the JSON shape Chrome reads for `override_update_url`.
    #[test]
    fn policy_values_round_trip_and_remove() {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;

        let forcelist = r"Software\SSDownloadPolicyTest\ExtensionInstallForcelist";
        let settings = r"Software\SSDownloadPolicyTest\ExtensionSettings";
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.delete_subkey_all(r"Software\SSDownloadPolicyTest")
            .ok();

        let report = install_policies_at(forcelist, settings).expect("policy write");
        assert!(report.installed, "{:?}", report.warnings);
        let key = hkcu.open_subkey(forcelist).expect("forcelist key");
        let values = key
            .enum_values()
            .flatten()
            .map(|(name, _)| key.get_value::<String, _>(&name).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![format!("{CHROME_EXTENSION_ID};{UPDATE_XML_URL}")]
        );
        drop(key);
        let key = hkcu.open_subkey(settings).expect("settings key");
        let value: String = key.get_value(CHROME_EXTENSION_ID).expect("settings value");
        assert!(value.contains(r#""override_update_url":true"#), "{value}");
        assert!(value.contains(UPDATE_XML_URL), "{value}");
        drop(key);
        assert!(policies_installed_at(forcelist).expect("install check"));
        remove_policies_at(forcelist, settings).expect("policy removal");
        assert!(!policies_installed_at(forcelist).expect("install check"));
        let key = hkcu.open_subkey(settings).expect("settings key");
        assert!(key.get_value::<String, _>(CHROME_EXTENSION_ID).is_err());
        drop(key);
        hkcu.delete_subkey_all(r"Software\SSDownloadPolicyTest")
            .ok();
    }

    /// The packaged CRX and the portable ZIP ship `browser/chromium`, so its
    /// manifest must carry the product version, this module's update URL and the
    /// key the fixed extension ID derives from.
    #[test]
    fn chromium_manifest_matches_the_product_version_and_identity() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use sha2::{Digest, Sha256};

        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../browser/chromium/manifest.json"))
                .expect("manifest.json geçerli JSON olmalı");
        assert_eq!(
            manifest["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "eklenti sürümü Cargo.toml ile aynı olmalı"
        );
        assert_eq!(
            manifest["update_url"].as_str(),
            Some(UPDATE_XML_URL),
            "manifest update_url ile UPDATE_XML_URL ayrışmamalı"
        );
        let key = manifest["key"].as_str().expect("manifest key eksik");
        let digest = Sha256::digest(STANDARD.decode(key).expect("manifest key base64 değil"));
        let derived = digest[..16]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .map(|nibble| char::from(b'a' + nibble))
            .collect::<String>();
        assert_eq!(
            derived, CHROME_EXTENSION_ID,
            "manifest anahtarından türeyen eklenti kimliği koddaki sabitle uyuşmuyor"
        );
    }
}
