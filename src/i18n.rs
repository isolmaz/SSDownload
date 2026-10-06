//! The interface language layer.
//!
//! The interface is Turkish by default; English is opt-in through the
//! persisted `Settings.ui_language` value (`"en"` after a trim, compared
//! case-insensitively, switches; every other value keeps Turkish). Machine
//! identifiers and codes are not localized; existing diagnostics retain their text.

use std::sync::atomic::{AtomicU8, Ordering};

/// The active interface language: `0` Turkish (the default), `1` English.
static LANGUAGE: AtomicU8 = AtomicU8::new(0);

/// Applies a stored `Settings.ui_language` value. Exactly `"en"` (trimmed,
/// compared case-insensitively) switches to English; every other value — an
/// empty string from a database written before the field existed included —
/// keeps Turkish.
pub(crate) fn set_language(value: &str) {
    let english = u8::from(value.trim().eq_ignore_ascii_case("en"));
    LANGUAGE.store(english, Ordering::Relaxed);
}

/// Whether English is the active interface language.
pub(crate) fn english() -> bool {
    LANGUAGE.load(Ordering::Relaxed) == 1
}

/// The active wording of a Turkish/English pair: Turkish before a switch,
/// English after one, each argument returned unchanged.
pub(crate) fn ui(tr: &'static str, en: &'static str) -> &'static str {
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
