//! The interface language layer.
//!
//! The language is the persisted `Settings.ui_language` value: `"en"` (trimmed, compared
//! case-insensitively) switches to English, every other value keeps Turkish. A profile that has
//! no settings yet starts in the Windows display language (`system_language`); a stored
//! choice is never replaced by it. The desktop applies the value when it opens the profile; the
//! CLI and the native-messaging host never open the app, so they read it with
//! `apply_profile_language` before they print anything. Machine identifiers and codes are
//! not localized; existing diagnostics retain their text.

use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
};

/// The active interface language: `0` Turkish (the default), `1` English.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

/// Primary language identifier of Turkish; a language ID keeps it in its low ten bits.
const LANG_TURKISH: u16 = 0x1f;

#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Applies a stored `Settings.ui_language` value. Exactly `"en"` (trimmed,
/// compared case-insensitively) switches to English; every other value — an
/// empty string from a database written before the field existed included —
/// keeps Turkish.
pub(crate) fn set_language(value: &str) {
    let english = u8::from(value.trim().eq_ignore_ascii_case("en"));
    LANGUAGE.store(english, Ordering::Relaxed);
}

/// Whether English is the active interface language.
pub fn english() -> bool {
    LANGUAGE.load(Ordering::Relaxed) == 1
}

/// The active wording of a Turkish/English pair: Turkish before a switch,
/// English after one, each argument returned unchanged.
pub fn ui(tr: &'static str, en: &'static str) -> &'static str {
    if english() {
        en
    } else {
        tr
    }
}

/// Select before formatting: callers must not allocate both language variants.
macro_rules! ui_owned {
    ($tr:expr, $en:expr $(,)?) => {{
        let text: String = if $crate::i18n::english() { $en } else { $tr };
        text
    }};
}
pub(crate) use ui_owned;

/// The `ui_language` value for a Windows language ID: Turkish for the Turkish primary
/// language, English for every other one.
fn language_of_langid(langid: u16) -> &'static str {
    if langid & 0x3ff == LANG_TURKISH {
        "tr"
    } else {
        "en"
    }
}

/// The `ui_language` a profile without stored settings starts with: the Windows display
/// language of the current user. Unit tests pin Turkish, whatever machine runs them.
pub(crate) fn system_language() -> &'static str {
    if cfg!(test) {
        return "tr";
    }
    language_of_langid(unsafe { GetUserDefaultUILanguage() })
}

fn stored_language(database: &Path) -> Option<String> {
    crate::store::Store::stored_settings(database).map(|settings| settings.ui_language)
}

/// Applies the interface language of the profile in `data_dir` (`None`: the default profile)
/// to a process that does not open the app. A profile without stored settings follows the
/// Windows display language. Nothing is created or modified.
pub fn apply_profile_language(data_dir: Option<&Path>) {
    let database = crate::paths::profile_base_dir(data_dir).join(crate::paths::DATABASE_FILE);
    match stored_language(&database) {
        Some(language) => set_language(&language),
        None => set_language(system_language()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_turkish_primary_language_selects_turkish() {
        assert_eq!(language_of_langid(0x041f), "tr");
        assert_eq!(language_of_langid(0x0409), "en");
        assert_eq!(language_of_langid(0x0807), "en");
        assert_eq!(
            language_of_langid(0x082c),
            "en",
            "Azerbaijani is not Turkish"
        );
    }

    #[test]
    fn the_stored_language_is_read_from_a_database_another_connection_holds_open() {
        let dir =
            std::env::temp_dir().join(format!("ssdownload-i18n-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let database = dir.join(crate::paths::DATABASE_FILE);
        assert_eq!(stored_language(&database), None, "no database yet");
        assert!(!database.exists(), "a lookup never creates the database");

        let mut store = crate::store::Store::open(&database).unwrap();
        assert_eq!(stored_language(&database), None, "no settings saved yet");
        let settings = crate::model::Settings {
            ui_language: "en".into(),
            ..Default::default()
        };
        store.save_settings(&settings).unwrap();
        assert_eq!(stored_language(&database).as_deref(), Some("en"));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
