//! The interface language layer.
//!
//! The language is the persisted `Settings.ui_language` value, one of the codes in
//! `LANGUAGES` (trimmed, compared case-insensitively); every other value — an empty string from
//! a database written before the field existed included — keeps Turkish. A profile that has no
//! settings yet starts in the Windows display language (`system_language`); a stored choice is
//! never replaced by it. The desktop applies the value when it opens the profile; the CLI and
//! the native-messaging host never open the app, so they read it with `apply_profile_language`
//! before they print anything. Machine identifiers and codes are not localized; existing
//! diagnostics retain their text.
//!
//! Every text is written where it is used as a Turkish/English pair (`ui`, `ui_format!`). The
//! other languages translate the English text through `i18n/<code>.json`, which
//! `scripts/i18n.py` compiles into `i18n_catalog.rs`; a text a catalog lacks stays English.

use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
};

/// An interface language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Turkish,
    English,
    Arabic,
    Spanish,
    Portuguese,
    French,
    German,
}

/// One entry of the language list.
pub struct LanguageInfo {
    pub language: Language,
    /// The `Settings.ui_language` value, also the code the browser extension receives.
    pub code: &'static str,
    /// The language's name in itself, as the settings list shows it.
    pub name: &'static str,
    /// The Windows primary language identifier that selects it for a new profile.
    primary: u16,
}

/// Every interface language, in the order the settings list shows them. Turkish comes first:
/// it is what an unknown stored value falls back to.
pub const LANGUAGES: [LanguageInfo; 7] = [
    LanguageInfo {
        language: Language::Turkish,
        code: "tr",
        name: "Türkçe",
        primary: 0x1f,
    },
    LanguageInfo {
        language: Language::English,
        code: "en",
        name: "English",
        primary: 0x09,
    },
    LanguageInfo {
        language: Language::Arabic,
        code: "ar",
        name: "العربية",
        primary: 0x01,
    },
    LanguageInfo {
        language: Language::Spanish,
        code: "es",
        name: "Español",
        primary: 0x0a,
    },
    LanguageInfo {
        language: Language::Portuguese,
        code: "pt",
        name: "Português",
        primary: 0x16,
    },
    LanguageInfo {
        language: Language::French,
        code: "fr",
        name: "Français",
        primary: 0x0c,
    },
    LanguageInfo {
        language: Language::German,
        code: "de",
        name: "Deutsch",
        primary: 0x07,
    },
];

/// The active interface language, as an index into `LANGUAGES`.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// The index into `LANGUAGES` a stored `Settings.ui_language` value selects.
fn index_of(value: &str) -> usize {
    let value = value.trim();
    LANGUAGES
        .iter()
        .position(|info| info.code.eq_ignore_ascii_case(value))
        .unwrap_or(0)
}

/// Applies a stored `Settings.ui_language` value.
pub(crate) fn set_language(value: &str) {
    LANGUAGE.store(index_of(value) as u8, Ordering::Relaxed);
}

/// The active interface language.
pub fn language() -> Language {
    LANGUAGES[usize::from(LANGUAGE.load(Ordering::Relaxed))].language
}

/// The code of the language a stored `Settings.ui_language` value selects.
pub fn code_of(value: &str) -> &'static str {
    LANGUAGES[index_of(value)].code
}

/// The code of the active interface language.
pub fn code() -> &'static str {
    LANGUAGES[usize::from(LANGUAGE.load(Ordering::Relaxed))].code
}

/// Whether the active language is written right to left, so windows are laid out mirrored.
pub fn right_to_left() -> bool {
    language() == Language::Arabic
}

/// The active wording of a Turkish/English pair. The other languages look the English text up
/// in their catalog and keep it where the catalog has no translation.
pub fn ui(tr: &'static str, en: &'static str) -> &'static str {
    match language() {
        Language::Turkish => tr,
        Language::English => en,
        other => translated(other, en).unwrap_or(en),
    }
}

fn translated(language: Language, en: &str) -> Option<&'static str> {
    let texts = crate::i18n_catalog::texts(language);
    texts
        .binary_search_by(|(english, _)| (*english).cmp(en))
        .ok()
        .map(|index| texts[index].1)
}

/// The active wording of a Turkish/English pair of `format!` templates, formatted with the
/// arguments that follow them; only the chosen template is formatted. The other languages
/// format their translation of the English template with the same arguments, so a
/// translation may reorder `{0}` and `{name}` placeholders but has to use each of them, or the
/// build fails. The English template is matched by its spelling, the one Rust's `{:?}` gives
/// it (`\n`, not a line break), and `scripts/i18n.py check` reports one spelled otherwise.
#[macro_export]
macro_rules! ui_format {
    ($tr:tt, $en:tt $(,)?) => {
        $crate::ui_format!(@pick $tr, $en, ())
    };
    ($tr:tt, $en:tt, $($arguments:tt)+) => {
        $crate::ui_format!(@pick $tr, $en, ($($arguments)+))
    };
    (@pick $tr:tt, $en:tt, ($($arguments:tt)*)) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::Turkish => format!($tr, $($arguments)*),
            $crate::i18n::Language::English => format!($en, $($arguments)*),
            $crate::i18n::Language::Arabic => {
                format!($crate::__ui_template_ar!($en), $($arguments)*)
            }
            $crate::i18n::Language::Spanish => {
                format!($crate::__ui_template_es!($en), $($arguments)*)
            }
            $crate::i18n::Language::Portuguese => {
                format!($crate::__ui_template_pt!($en), $($arguments)*)
            }
            $crate::i18n::Language::French => {
                format!($crate::__ui_template_fr!($en), $($arguments)*)
            }
            $crate::i18n::Language::German => {
                format!($crate::__ui_template_de!($en), $($arguments)*)
            }
        }
    };
}
pub use crate::ui_format;

/// The `ui_language` value for a Windows language ID: the language whose primary identifier
/// it carries in its low ten bits, English for one the app does not offer.
fn language_of_langid(langid: u16) -> &'static str {
    LANGUAGES
        .iter()
        .find(|info| info.primary == langid & 0x3ff)
        .map_or("en", |info| info.code)
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
    fn the_windows_primary_language_selects_the_matching_interface_language() {
        assert_eq!(language_of_langid(0x041f), "tr");
        assert_eq!(language_of_langid(0x0409), "en");
        assert_eq!(language_of_langid(0x0807), "de", "Swiss German is German");
        assert_eq!(
            language_of_langid(0x0c01),
            "ar",
            "Egyptian Arabic is Arabic"
        );
        assert_eq!(
            language_of_langid(0x0816),
            "pt",
            "European Portuguese gets Portuguese"
        );
        assert_eq!(language_of_langid(0x080a), "es");
        assert_eq!(language_of_langid(0x0c0c), "fr");
        assert_eq!(
            language_of_langid(0x082c),
            "en",
            "Azerbaijani is not Turkish"
        );
        assert_eq!(language_of_langid(0x0411), "en", "Japanese is not offered");
    }

    #[test]
    fn a_stored_value_selects_its_language_and_anything_else_keeps_turkish() {
        assert_eq!(code_of(" DE "), "de");
        assert_eq!(code_of("pt"), "pt");
        assert_eq!(
            code_of(""),
            "tr",
            "a database written before the field existed"
        );
        assert_eq!(code_of("pt-BR"), "tr");
        assert_eq!(code_of("xx"), "tr");
    }

    #[test]
    fn every_catalog_is_sorted_and_finds_each_of_its_texts() {
        for info in &LANGUAGES[2..] {
            let texts = crate::i18n_catalog::texts(info.language);
            assert!(texts.len() > 1000, "{} has a catalog", info.code);
            assert!(
                texts.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "{} is sorted for the binary search",
                info.code
            );
            for (english, translation) in texts {
                assert_eq!(translated(info.language, english), Some(*translation));
            }
            assert_eq!(translated(info.language, "no such text"), None);
        }
    }

    #[test]
    fn a_translated_template_formats_the_same_arguments() {
        let german = format!(crate::__ui_template_de!("{0} connections"), 4);
        assert!(
            german.contains('4') && !german.contains("connections"),
            "{german}"
        );
        assert_eq!(
            format!(crate::__ui_template_de!("no such template {0}"), 1),
            "no such template 1",
            "a template the catalog lacks stays English"
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
