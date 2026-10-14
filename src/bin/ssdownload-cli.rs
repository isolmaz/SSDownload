//! Console-subsystem façade for the GUI-subsystem desktop executable.
//!
//! PowerShell does not wait for a GUI-subsystem child invoked with `&`, which closes a redirected
//! stdout pipe before `ssdownload.exe` can answer. This executable is intentionally a very small
//! console process: it forwards every argument and inherited stream, then waits for the adjacent
//! GUI executable and returns its exact exit status. The only thing it reads is the profile's
//! interface language, so its own failure message matches the app's wording.

use ssdownload::i18n::{apply_profile_language, ui};
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn main() {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    // The failures below are worded in the language of the profile the desktop executable will use.
    apply_profile_language(data_dir(&arguments).as_deref());
    match run(arguments) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("SSDownload CLI: {error}");
            std::process::exit(1);
        }
    }
}

/// The profile named by `--data-dir`; `None` addresses the default profile.
fn data_dir(arguments: &[OsString]) -> Option<PathBuf> {
    let index = arguments
        .iter()
        .position(|argument| argument.as_os_str() == "--data-dir")?;
    arguments.get(index + 1).map(PathBuf::from)
}

fn run(arguments: Vec<OsString>) -> Result<i32, Box<dyn std::error::Error>> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE},
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };
        // Command duplicates the selected standard streams. Do not also pass
        // their original inheritable handles to the desktop's descendants.
        for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(stream);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    let wrapper = std::env::current_exe()?;
    let desktop = wrapper.with_file_name("ssdownload.exe");
    if !desktop.is_file() {
        return Err(format!(
            "{}: {}",
            ui(
                "Yanındaki ssdownload.exe bulunamadı",
                "ssdownload.exe was not found next to this program"
            ),
            desktop.display()
        )
        .into());
    }
    let mut command = Command::new(desktop);
    command
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}
