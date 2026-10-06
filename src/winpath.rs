//! Extended-length path conversion for raw Win32 calls.
//!
//! Rust's standard library adds the `\\?\` prefix itself, but `windows-sys`
//! calls receive exactly the buffer they are given. A download path built from
//! an ordinary directory plus a 180-character file name can exceed `MAX_PATH`,
//! and an unprefixed buffer then fails with `ERROR_PATH_NOT_FOUND` even though
//! the file is reachable through Rust's own APIs.

use std::{ffi::OsStr, path::Path};

/// NUL-terminated UTF-16 buffer for a Win32 path parameter.
///
/// A fully qualified drive or UNC path is converted to its `\\?\` form so
/// calls keep working beyond `MAX_PATH`. Anything that cannot be expressed as a
/// verbatim path (relative paths, forward slashes, `.`/`..` components, device
/// paths we do not own) is passed through unchanged, which preserves today's
/// behaviour instead of inventing an invalid path.
pub(crate) fn wide_long(path: &Path) -> Vec<u16> {
    prefix_verbatim(path.as_os_str())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn prefix_verbatim(value: &OsStr) -> String {
    let text = value.to_string_lossy();
    let extended = if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        None
    } else if let Some(rest) = text.strip_prefix(r"\\") {
        Some(format!(r"\\?\UNC\{rest}"))
    } else if is_drive_qualified(&text) {
        Some(format!(r"\\?\{text}"))
    } else {
        None
    };
    match extended {
        Some(extended) if verbatim_safe(&extended) => extended,
        _ => text.into_owned(),
    }
}

/// Path text for shell calls (`ShellExecute` and friends).
///
/// The shell cannot open a verbatim (`\\?\`) path, and for an empty or root-only target it
/// shows its own "Windows cannot find" dialog that the application cannot log. Returning an
/// error here keeps the failure inside the application, where it reaches the message list and
/// the event log, and hands the shell a normal path.
pub(crate) fn shell_target(path: &Path) -> Result<String, String> {
    let text = path.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Açılacak yol boş".into());
    }
    let plain = if let Some(rest) = trimmed.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        trimmed.to_owned()
    };
    // The empty check above runs before the verbatim strip, so a bare `\\?\`
    // would otherwise reach the shell as "" and surface its own failure dialog.
    if plain.is_empty() {
        return Err("Açılacak yol boş".into());
    }
    let only_separators = |value: &str| {
        !value.is_empty()
            && value
                .chars()
                .all(|character| character == '\\' || character == '/')
    };
    if only_separators(&plain) {
        return Err(format!("Açılacak yol geçersiz: {plain}"));
    }
    Ok(plain)
}

fn is_drive_qualified(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Verbatim paths skip Win32 normalization, so they must already be plain:
/// backslashes only, without `.` or `..` components.
fn verbatim_safe(value: &str) -> bool {
    if value.contains('/') {
        return false;
    }
    let body = value
        .strip_prefix(r"\\?\")
        .or_else(|| value.strip_prefix(r"\\?\UNC\"))
        .unwrap_or(value);
    !body
        .split('\\')
        .any(|component| component == "." || component == "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn text(path: &str) -> String {
        let wide = wide_long(Path::new(path));
        String::from_utf16(&wide[..wide.len() - 1]).expect("UTF-16")
    }

    #[test]
    fn fully_qualified_paths_gain_the_extended_prefix() {
        assert_eq!(
            text(r"C:\Users\adam\Downloads\file.bin"),
            r"\\?\C:\Users\adam\Downloads\file.bin"
        );
        assert_eq!(
            text(r"\\server\share\folder\file.bin"),
            r"\\?\UNC\server\share\folder\file.bin"
        );
    }

    #[test]
    fn the_shell_receives_a_plain_path_or_a_reason() {
        assert_eq!(
            shell_target(Path::new(r"\\?\C:\Users\adam\Downloads")).expect("verbatim"),
            r"C:\Users\adam\Downloads"
        );
        assert_eq!(
            shell_target(Path::new(r"\\?\UNC\server\share")).expect("unc verbatim"),
            r"\\server\share"
        );
        assert_eq!(
            shell_target(Path::new(r"C:\Users\adam")).expect("plain"),
            r"C:\Users\adam"
        );
        assert!(shell_target(Path::new("")).is_err());
        assert!(shell_target(Path::new(r"\\")).is_err());
        assert!(shell_target(Path::new(r"\")).is_err());
    }

    #[test]
    fn already_verbatim_paths_are_left_alone() {
        assert_eq!(
            text(r"\\?\C:\Users\adam\file.bin"),
            r"\\?\C:\Users\adam\file.bin"
        );
        assert_eq!(
            text(r"\\?\UNC\server\share\file.bin"),
            r"\\?\UNC\server\share\file.bin"
        );
    }

    #[test]
    fn unconvertible_paths_stay_usable() {
        for value in [
            r"relative\file.bin",
            r"C:drive-relative.bin",
            r"C:\mixed/forward.bin",
            r"C:\folder\..\escape.bin",
            r"C:\folder\.\current.bin",
            r"\\?\C:\already\verbatim\..\odd.bin",
            "",
        ] {
            assert_eq!(text(value), value, "unexpected conversion for {value}");
        }
    }

    #[test]
    fn buffers_are_nul_terminated() {
        let wide = wide_long(Path::new(r"C:\file.bin"));
        assert_eq!(wide.last().copied(), Some(0));
    }
}
