//! Browser extension distribution for the fixed SSDownload extension.
//!
//! The extension is listed on the Chrome Web Store under the ID its manifest
//! `key` derives. Registration writes Chromium's documented "external
//! extension" entries (`HKCU\Software\Google\Chrome\Extensions\<id>` and the
//! Edge equivalent) with the Web Store update URL: on the next start the
//! browser downloads the extension and asks the user once to enable it.
//! Windows Chrome and Edge accept such entries only for store-hosted
//! extensions, which is why the self-hosted CRX channel is gone. Removing the
//! entries (uninstall) makes the browser remove the extension again.
//!
//! Earlier versions wrote `ExtensionInstallForcelist` policies pointing at a
//! self-hosted update manifest. On devices Chrome does not manage, such an
//! entry is `[BLOCKED]` and blocks the extension ID outright, so leftovers are
//! removed (`remove_legacy_policies`).

use anyhow::{Context, Result};

pub const CHROME_EXTENSION_ID: &str = "hgndggnlfpnflkmnbddmcnfniamckham";
/// Every extension identity the native host answers. The Chrome Web Store
/// listing keeps the manifest-key ID; an Edge Add-ons listing gets its own ID,
/// which must be added here once the listing exists.
pub const ALLOWED_EXTENSION_IDS: &[&str] = &[CHROME_EXTENSION_ID];
/// Chrome Web Store update service; Edge accepts it for registry installs too.
pub const WEB_STORE_UPDATE_URL: &str = "https://clients2.google.com/service/update2/crx";
/// Public Chrome Web Store listing of the extension.
pub const WEB_STORE_LISTING_URL: &str =
    "https://chromewebstore.google.com/detail/hgndggnlfpnflkmnbddmcnfniamckham";

/// Per-user external extension roots read by Chrome (and Chromium forks that
/// keep Chrome's key) and by Microsoft Edge.
const EXTERNAL_EXTENSION_ROOTS: [&str; 2] = [
    r"Software\Google\Chrome\Extensions",
    r"Software\Microsoft\Edge\Extensions",
];

/// Legacy per-user policy keys written by versions up to 1.6.1.
const LEGACY_POLICY_KEYS: [(&str, &str); 2] = [
    (
        r"Software\Policies\Google\Chrome\ExtensionInstallForcelist",
        r"Software\Policies\Google\Chrome\ExtensionSettings",
    ),
    (
        r"Software\Policies\Microsoft\Edge\ExtensionInstallForcelist",
        r"Software\Policies\Microsoft\Edge\ExtensionSettings",
    ),
];

/// Registers the Web Store extension with Chrome and Edge for the current user.
pub fn register_external() -> Result<()> {
    register_external_at(&EXTERNAL_EXTENSION_ROOTS)
}

fn register_external_at(roots: &[&str]) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    for root in roots {
        let (key, _) = hkcu
            .create_subkey(format!(r"{root}\{CHROME_EXTENSION_ID}"))
            .with_context(|| format!(r"HKCU\{root} could not be written"))?;
        key.set_value("update_url", &WEB_STORE_UPDATE_URL)
            .with_context(|| format!(r"HKCU\{root} could not be written"))?;
    }
    Ok(())
}

/// Removes the external extension entries; the browsers then uninstall the
/// extension on their next start.
pub fn unregister_external() -> Result<()> {
    unregister_external_at(&EXTERNAL_EXTENSION_ROOTS)
}

fn unregister_external_at(roots: &[&str]) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    for root in roots {
        match hkcu.delete_subkey_all(format!(r"{root}\{CHROME_EXTENSION_ID}")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!(r"HKCU\{root} could not be cleaned"))
            }
        }
    }
    Ok(())
}

fn is_our_forcelist_data(data: &str) -> bool {
    data.split(';').next() == Some(CHROME_EXTENSION_ID)
}

/// True when a legacy force-install policy for our ID is still present.
pub fn legacy_policies_present() -> bool {
    LEGACY_POLICY_KEYS
        .iter()
        .any(|(forcelist, _)| forcelist_entry_present(forcelist))
}

fn forcelist_entry_present(forcelist_key: &str) -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey(forcelist_key) else {
        return false;
    };
    key.enum_values().flatten().any(|(name, _)| {
        key.get_value::<String, _>(&name)
            .is_ok_and(|data| is_our_forcelist_data(&data))
    })
}

/// Outcome of a legacy policy clean-up.
#[derive(Debug, PartialEq, Eq)]
pub enum LegacyCleanup {
    NothingFound,
    Removed,
    /// The policy hive refuses the delete; an elevated retry is required.
    NeedsElevation,
}

impl LegacyCleanup {
    pub fn summary(&self) -> &'static str {
        match self {
            LegacyCleanup::NothingFound => crate::i18n::ui(
                "Eski eklenti politikası kaydı bulunamadı.",
                "No legacy extension policy entry was found.",
            ),
            LegacyCleanup::Removed => crate::i18n::ui(
                "Eski eklenti politikası kaydı kaldırıldı.",
                "The legacy extension policy entry was removed.",
            ),
            LegacyCleanup::NeedsElevation => crate::i18n::ui(
                "Eski eklenti politikası kaydı yönetici izni olmadan kaldırılamadı.",
                "The legacy extension policy entry could not be removed without administrator rights.",
            ),
        }
    }
}

/// Removes the force-install entries for our ID that versions up to 1.6.1
/// wrote. Values owned by other software are never touched.
pub fn remove_legacy_policies() -> LegacyCleanup {
    let mut outcome = LegacyCleanup::NothingFound;
    for (forcelist, settings) in LEGACY_POLICY_KEYS {
        match remove_policy_at(forcelist, settings) {
            LegacyCleanup::NothingFound => {}
            LegacyCleanup::Removed => {
                if outcome == LegacyCleanup::NothingFound {
                    outcome = LegacyCleanup::Removed;
                }
            }
            LegacyCleanup::NeedsElevation => outcome = LegacyCleanup::NeedsElevation,
        }
    }
    outcome
}

fn remove_policy_at(forcelist_key: &str, settings_key: &str) -> LegacyCleanup {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let mut removed = false;
    let mut denied = false;
    if let Ok(key) = hkcu.open_subkey(settings_key) {
        if key.get_value::<String, _>(CHROME_EXTENSION_ID).is_ok() {
            match hkcu
                .open_subkey_with_flags(settings_key, KEY_SET_VALUE)
                .and_then(|key| key.delete_value(CHROME_EXTENSION_ID))
            {
                Ok(()) => removed = true,
                Err(_) => denied = true,
            }
        }
    }
    if forcelist_entry_present(forcelist_key) {
        match hkcu.open_subkey_with_flags(forcelist_key, KEY_SET_VALUE | KEY_QUERY_VALUE) {
            Ok(key) => {
                let names = key
                    .enum_values()
                    .flatten()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>();
                for name in names {
                    let ours = key
                        .get_value::<String, _>(&name)
                        .is_ok_and(|data| is_our_forcelist_data(&data));
                    if ours {
                        match key.delete_value(&name) {
                            Ok(()) => removed = true,
                            Err(_) => denied = true,
                        }
                    }
                }
            }
            Err(_) => denied = true,
        }
    }
    if denied {
        LegacyCleanup::NeedsElevation
    } else if removed {
        LegacyCleanup::Removed
    } else {
        LegacyCleanup::NothingFound
    }
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
            crate::i18n::ui_format!(
                "Yönetici izniyle çalıştırma reddedildi (kod {0})",
                "Running as administrator was denied (code {0})",
                result as isize
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    const SCRATCH: &str = r"Software\SSDownloadExtensionTest";

    /// Registration writes exactly the value Chromium's registry loader reads
    /// (`update_url` under `<root>\<id>`), and removal deletes only our ID.
    #[test]
    fn external_entries_round_trip_without_touching_other_extensions() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let scratch = format!(r"{SCRATCH}\{}", uuid::Uuid::new_v4().simple());
        let chrome = format!(r"{scratch}\Chrome\Extensions");
        let edge = format!(r"{scratch}\Edge\Extensions");
        let foreign = format!(r"{chrome}\aaaabbbbccccddddeeeeffffgggghhhh");
        hkcu.create_subkey(&foreign)
            .unwrap()
            .0
            .set_value("update_url", &"https://example.test/crx")
            .unwrap();

        register_external_at(&[&chrome, &edge]).expect("register");
        register_external_at(&[&chrome, &edge]).expect("register is idempotent");
        for root in [&chrome, &edge] {
            let key = hkcu
                .open_subkey(format!(r"{root}\{CHROME_EXTENSION_ID}"))
                .expect("extension key");
            let url: String = key.get_value("update_url").unwrap();
            assert_eq!(url, WEB_STORE_UPDATE_URL);
        }

        unregister_external_at(&[&chrome, &edge]).expect("unregister");
        unregister_external_at(&[&chrome, &edge]).expect("unregister when absent");
        for root in [&chrome, &edge] {
            assert!(hkcu
                .open_subkey(format!(r"{root}\{CHROME_EXTENSION_ID}"))
                .is_err());
        }
        assert!(hkcu.open_subkey(&foreign).is_ok(), "foreign entry removed");
        hkcu.delete_subkey_all(&scratch).unwrap();
        // Fails while a parallel test still owns a sibling scratch key.
        hkcu.delete_subkey(SCRATCH).ok();
    }

    /// A leftover force-list entry for our ID is removed together with its
    /// settings value; another product's entries in the same keys survive.
    #[test]
    fn legacy_policy_cleanup_removes_only_our_entries() {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let scratch = format!(r"{SCRATCH}\{}", uuid::Uuid::new_v4().simple());
        let forcelist = format!(r"{scratch}\ExtensionInstallForcelist");
        let settings = format!(r"{scratch}\ExtensionSettings");
        let (list, _) = hkcu.create_subkey(&forcelist).unwrap();
        list.set_value(
            "1",
            &"otherotherotherotherotherotherot;https://x.test/u.xml",
        )
        .unwrap();
        list.set_value(
            "2",
            &format!("{CHROME_EXTENSION_ID};https://example.test/update.xml"),
        )
        .unwrap();
        let (values, _) = hkcu.create_subkey(&settings).unwrap();
        values.set_value(CHROME_EXTENSION_ID, &"{}").unwrap();
        values
            .set_value("otherotherotherotherotherotherot", &"{}")
            .unwrap();

        assert!(forcelist_entry_present(&forcelist));
        assert_eq!(
            remove_policy_at(&forcelist, &settings),
            LegacyCleanup::Removed
        );
        assert!(!forcelist_entry_present(&forcelist));
        assert_eq!(
            remove_policy_at(&forcelist, &settings),
            LegacyCleanup::NothingFound
        );
        assert!(list.get_value::<String, _>("1").is_ok());
        assert!(values.get_value::<String, _>(CHROME_EXTENSION_ID).is_err());
        assert!(values
            .get_value::<String, _>("otherotherotherotherotherotherot")
            .is_ok());
        drop((list, values));
        hkcu.delete_subkey_all(&scratch).unwrap();
        // Fails while a parallel test still owns a sibling scratch key.
        hkcu.delete_subkey(SCRATCH).ok();
    }

    /// The Web Store listing, the native host allow-list and the registry
    /// entries all use the ID derived from the manifest `key`, and the
    /// manifest carries the product version.
    #[test]
    fn chromium_manifest_matches_the_product_version_and_identity() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use sha2::{Digest, Sha256};

        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../browser/chromium/manifest.json"))
                .expect("manifest.json must be valid JSON");
        assert_eq!(
            manifest["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "the extension version must match Cargo.toml"
        );
        assert!(
            manifest.get("update_url").is_none(),
            "a self-hosted update_url must not return; the Web Store serves updates"
        );
        let key = manifest["key"].as_str().expect("manifest key missing");
        let digest = Sha256::digest(STANDARD.decode(key).expect("manifest key is not base64"));
        let derived = digest[..16]
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .map(|nibble| char::from(b'a' + nibble))
            .collect::<String>();
        assert_eq!(derived, CHROME_EXTENSION_ID);
        assert!(WEB_STORE_LISTING_URL.ends_with(CHROME_EXTENSION_ID));
    }
}
