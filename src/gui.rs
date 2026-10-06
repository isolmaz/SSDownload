#[cfg(windows)]
mod native;

#[cfg(windows)]
pub use native::run;

use crate::update::{UpdateDecision, UpdateManifest, UpdateOutcome};

/// Native MB_YESNO update offer. Returns None only on non-Windows builds,
/// where the desktop updater is unavailable.
#[cfg(windows)]
pub(crate) fn offer_update(manifest: &UpdateManifest) -> Option<UpdateDecision> {
    native::offer_update(manifest)
}
#[cfg(not(windows))]
pub(crate) fn offer_update(_manifest: &UpdateManifest) -> Option<UpdateDecision> {
    None
}

/// Surfaces the updater result as a native message (dialog or tray notify).
#[cfg(windows)]
pub(crate) fn report_update_outcome(outcome: &UpdateOutcome) {
    native::report_update_outcome(outcome)
}
#[cfg(not(windows))]
pub(crate) fn report_update_outcome(_outcome: &UpdateOutcome) {}

/// Plain informational update message ("already up to date", feed errors).
#[cfg(windows)]
pub(crate) fn report_update_text(text: &str) {
    native::report_update_text(text)
}
#[cfg(not(windows))]
pub(crate) fn report_update_text(_text: &str) {}

/// Plain update error message.
#[cfg(windows)]
pub(crate) fn report_update_error(text: &str) {
    native::report_update_error(text)
}
#[cfg(not(windows))]
pub(crate) fn report_update_error(_text: &str) {}

/// One-time startup offer to install the browser extension policies.
#[cfg(windows)]
pub(crate) fn offer_extension_install(app: crate::app::App) {
    native::offer_extension_install(app)
}
#[cfg(not(windows))]
pub(crate) fn offer_extension_install(_app: crate::app::App) {}

/// Monitor rect of the window that owns the foreground when a browser handoff
/// arrives: the native picker opens where the browser is. `None` when no window
/// on this desktop is in the foreground.
#[cfg(windows)]
pub(crate) fn foreground_monitor() -> Option<[i32; 4]> {
    native::foreground_monitor()
}
#[cfg(not(windows))]
pub(crate) fn foreground_monitor() -> Option<[i32; 4]> {
    None
}

#[cfg(not(windows))]
pub fn run(_app: crate::app::App, _start_hidden: bool) -> anyhow::Result<()> {
    anyhow::bail!("SSDownload masaüstü arayüzü yalnızca Windows'ta kullanılabilir")
}
