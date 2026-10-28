use crate::{
    app::App,
    i18n::ui,
    model::{Action, AppSnapshot, BridgeResponse, Settings},
    paths::AppPaths,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr, thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
        ERROR_NO_DATA, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, FILETIME,
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, RevertToSelf, TokenUser, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
            ImpersonateNamedPipeClient, PeekNamedPipe, SetNamedPipeHandleState, TransactNamedPipe,
            PIPE_NOWAIT, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
            PIPE_WAIT,
        },
        Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
            KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        },
        Threading::{
            GetCurrentProcess, GetCurrentThread, GetProcessTimes, OpenProcess, OpenProcessToken,
            OpenThreadToken, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::WindowsAndMessaging::{AllowSetForegroundWindow, WM_APP},
};

const PROTOCOL_VERSION: u32 = 2;
const HOST_NAME: &str = "com.ssdownload.desktop";
const MAX_NATIVE_MESSAGE: usize = 1024 * 1024;
const MAX_PIPE_MESSAGE: usize = 1024 * 1024;
/// Ceiling for the page markup a browser handoff may carry for error reports.
/// The extension truncates below this; the bridge refuses anything larger.
const MAX_PAGE_HTML: usize = 256 * 1024;
/// Most cookies a browser handoff may carry, and the longest file name it may suggest. Both
/// mirror what the extension itself enforces before it sends anything.
const MAX_BROWSER_COOKIES: usize = 500;
const MAX_BROWSER_FILENAME: usize = 1024;
const PIPE_WORKERS: u32 = 8;
const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(15);
/// How long a client waits for the pipe name to reappear while the server swaps instances. It
/// only covers failures observed before a request could have been written to the pipe.
const PIPE_RECONNECT_WINDOW: Duration = Duration::from_millis(250);
/// How long a new instance waits for the previous one to release the pipe name before it gives up
/// creating its first listener.
const PIPE_NAME_RELEASE_WINDOW: Duration = Duration::from_secs(5);
/// Main window of the desktop instance as another process sees it: the class name it
/// registers (`gui/native/mod.rs`) and the message that makes it raise that window exactly the
/// way `Action::ShowWindow` does. The pipe is what a second launch normally uses, so both
/// values are a cross-process contract and live here, next to the foreground grant.
pub const MAIN_WINDOW_CLASS: &str = "SSDownload.MainWindow";
pub const WM_RAISE_WINDOW: u32 = WM_APP + 36;

/// The reply to a request that speaks another protocol version than this build.
fn unsupported_version(version: u32) -> String {
    crate::i18n::ui_format!(
        "Desteklenmeyen iletişim sürümü: {version}",
        "Unsupported protocol version: {version}",
        version = version
    )
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    version: u32,
    #[serde(default)]
    action: Option<Action>,
}

struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct PipeSecurity(*mut c_void);
unsafe impl Send for PipeSecurity {}
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win32_error(tr: &'static str, en: &'static str) -> anyhow::Error {
    let code = unsafe { GetLastError() };
    let context = ui(tr, en);
    anyhow!(
        "{}",
        crate::i18n::ui_format!(
            "{context} (Windows hata kodu {code})",
            "{context} (Windows error code {code})",
            code = code,
            context = context
        )
    )
}

fn token_user_sid(token: windows_sys::Win32::Foundation::HANDLE) -> Result<Vec<u8>> {
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(win32_error(
            "Kullanıcı güvenlik kimliği okunamadı",
            "The user security ID could not be read",
        ));
    }
    let mut data = vec![0u8; needed as usize];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            data.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(win32_error(
            "Kullanıcı güvenlik kimliği okunamadı",
            "The user security ID could not be read",
        ));
    }
    Ok(data)
}

fn current_user_sid_string() -> Result<String> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(win32_error(
            "Kullanıcı erişim belirteci açılamadı",
            "The user access token could not be opened",
        ));
    }
    let token = OwnedHandle(token);
    let data = token_user_sid(token.0)?;
    let user = unsafe { data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
    let mut sid_text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_text) } == 0 {
        return Err(win32_error(
            "Kullanıcı güvenlik kimliği dönüştürülemedi",
            "The user security ID could not be converted",
        ));
    }
    let mut length = 0;
    unsafe {
        while *sid_text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, length) });
    unsafe { LocalFree(sid_text.cast()) };
    result.context(ui(
        "Kullanıcı güvenlik kimliği geçersiz UTF-16 içeriyor",
        "The user security ID contains invalid UTF-16",
    ))
}

fn pipe_security() -> Result<PipeSecurity> {
    let sid = current_user_sid_string()?;
    // Protected DACL: only this user's SID can connect. No inherited broad ACLs.
    let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));
    let mut descriptor = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(win32_error(
            "Named pipe güvenlik tanımı oluşturulamadı",
            "The named pipe security descriptor could not be created",
        ));
    }
    Ok(PipeSecurity(descriptor))
}

fn create_pipe(name: &str, security: &PipeSecurity, first: bool) -> Result<OwnedHandle> {
    create_pipe_within(name, security, first, PIPE_NAME_RELEASE_WINDOW)
}

/// Creates one pipe instance. The first instance claims the name exclusively, and Windows
/// refuses that with `ERROR_ACCESS_DENIED` while any other handle still holds an instance of the
/// name, as the bridge threads of a previous instance do for a moment after it released the
/// single-instance lock; so the first instance is retried for up to `wait` before it fails.
fn create_pipe_within(
    name: &str,
    security: &PipeSecurity,
    first: bool,
    wait: Duration,
) -> Result<OwnedHandle> {
    let name = wide(name);
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let deadline = Instant::now() + wait;
    loop {
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        0
                    },
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_WORKERS,
                MAX_PIPE_MESSAGE as u32,
                MAX_PIPE_MESSAGE as u32,
                PIPE_IO_TIMEOUT.as_millis() as u32,
                &attributes,
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(OwnedHandle(handle));
        }
        if first && unsafe { GetLastError() } == ERROR_ACCESS_DENIED && Instant::now() < deadline {
            thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }
        return Err(win32_error(
            "SSDownload yerel iletişim kanalı oluşturulamadı",
            "The SSDownload local communication channel could not be created",
        ));
    }
}

fn same_user_client(pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<bool> {
    // Clients connect at the identification level: their token can be read, but while it is
    // impersonated no object can be opened, so this process's own token is read before.
    let mut server_token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut server_token) } == 0 {
        return Err(win32_error(
            "Sunucu erişim belirteci okunamadı",
            "The server access token could not be read",
        ));
    }
    let server_token = OwnedHandle(server_token);
    let server_data = token_user_sid(server_token.0)?;
    let server_user = unsafe { server_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(win32_error(
            "Named pipe istemcisi doğrulanamadı",
            "The named pipe client could not be verified",
        ));
    }
    let result = (|| {
        let mut client_token = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut client_token) } == 0 {
            return Err(win32_error(
                "İstemci erişim belirteci okunamadı",
                "The client access token could not be read",
            ));
        }
        let client_token = OwnedHandle(client_token);
        let client_data = token_user_sid(client_token.0)?;
        let client_user = unsafe { client_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
        Ok(unsafe {
            windows_sys::Win32::Security::EqualSid(client_user.User.Sid, server_user.User.Sid) != 0
        })
    })();
    if unsafe { RevertToSelf() } == 0 {
        return Err(win32_error(
            "Named pipe kimliğinden çıkılamadı",
            "The named pipe identity could not be reverted",
        ));
    }
    result
}

/// Confirms `pid` runs under this user's identity before anything is handed to it, a request
/// or a foreground permission. The pipe DACL limits who may connect to a server this application
/// created; it says nothing about a server another process created under the same name, so
/// the identity comes from the process itself, not from text.
fn same_user_process(pid: u32) -> Result<bool> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(win32_error(
            "Named pipe sunucu süreci açılamadı",
            "The named pipe server process could not be opened",
        ));
    }
    let process = OwnedHandle(process);
    let mut endpoint_token = ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut endpoint_token) } == 0 {
        return Err(win32_error(
            "Sunucu erişim belirteci okunamadı",
            "The server access token could not be read",
        ));
    }
    let endpoint_token = OwnedHandle(endpoint_token);
    let endpoint_data = token_user_sid(endpoint_token.0)?;
    let endpoint_user = unsafe { endpoint_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };

    let mut own_token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own_token) } == 0 {
        return Err(win32_error(
            "Kullanıcı erişim belirteci açılamadı",
            "The user access token could not be opened",
        ));
    }
    let own_token = OwnedHandle(own_token);
    let own_data = token_user_sid(own_token.0)?;
    let own_user = unsafe { own_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
    Ok(unsafe {
        windows_sys::Win32::Security::EqualSid(endpoint_user.User.Sid, own_user.User.Sid) != 0
    })
}

fn wait_for_message(
    handle: windows_sys::Win32::Foundation::HANDLE,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut available = 0u32;
        let mut message_left = 0u32;
        if unsafe {
            PeekNamedPipe(
                handle,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut available,
                &mut message_left,
            )
        } == 0
        {
            return Err(win32_error(
                "Named pipe verisi okunamadı",
                "The named pipe data could not be read",
            ));
        }
        let size = available.max(message_left) as usize;
        if size > MAX_PIPE_MESSAGE {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Yerel iletişim iletisi {0} bayt sınırını aşıyor",
                    "The local message exceeds the {0} byte limit",
                    MAX_PIPE_MESSAGE
                )
            );
        }
        if size > 0 {
            let mut data = vec![0u8; size];
            let mut read = 0u32;
            if unsafe {
                ReadFile(
                    handle,
                    data.as_mut_ptr(),
                    data.len() as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(win32_error(
                    "Named pipe iletisi okunamadı",
                    "The named pipe message could not be read",
                ));
            }
            data.truncate(read as usize);
            return Ok(data);
        }
        if Instant::now() >= deadline {
            crate::bail_code!(crate::error_codes::BRG_005);
        }
        thread::sleep(PIPE_POLL_INTERVAL);
    }
}

fn write_message(handle: windows_sys::Win32::Foundation::HANDLE, data: &[u8]) -> Result<()> {
    if data.len() > MAX_PIPE_MESSAGE {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Yerel iletişim yanıtı {0} bayt sınırını aşıyor",
                "The local reply exceeds the {0} byte limit",
                MAX_PIPE_MESSAGE
            )
        );
    }
    let mut written = 0u32;
    if unsafe {
        WriteFile(
            handle,
            data.as_ptr(),
            data.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(win32_error(
            "Named pipe yanıtı yazılamadı",
            "The named pipe reply could not be written",
        ));
    }
    if written as usize != data.len() {
        bail!(
            "{}",
            ui(
                "Named pipe yanıtı eksik yazıldı",
                "The named pipe reply was written incompletely"
            )
        );
    }
    Ok(())
}

/// Decides whether the process with this id is the current user's.
type OwnerCheck = fn(u32) -> Result<bool>;

/// Opens one message-mode connection to the desktop pipe as this user. Nothing is written
/// before the process serving the pipe is known to be this user's (see `verify_pipe_server`).
/// Retrying is limited to failures observed before anything is written; a connection that was
/// established is never reopened, so a request is transmitted at most once.
fn connect_pipe(paths: &AppPaths) -> Result<OwnedHandle> {
    connect_pipe_checked(paths, same_user_process)
}

/// `connect_pipe` with the identity check as a parameter, so the refusal of a foreign server
/// can be exercised without a second Windows user.
fn connect_pipe_checked(paths: &AppPaths, owner_is_ours: OwnerCheck) -> Result<OwnedHandle> {
    let pipe_name = wide(&paths.pipe_name());
    let started = Instant::now();
    loop {
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                // The server may be any process that created the name first. At the
                // identification level it can learn who connects but never act as this user.
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            let pipe = OwnedHandle(handle);
            verify_pipe_server(pipe.0, owner_is_ours)?;
            // The server end is a message pipe and the client defaults to byte mode, where a
            // read can return a partial message, and `TransactNamedPipe` needs message mode.
            let mode = PIPE_READMODE_MESSAGE;
            if unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) } == 0 {
                return Err(win32_error(
                    "Named pipe okuma kipi ayarlanamadı",
                    "The named pipe read mode could not be set",
                ));
            }
            return Ok(pipe);
        }
        let code = unsafe { GetLastError() };
        // The name is missing while the server swaps instances; it is busy while every
        // instance serves a client, which lasts as long as one exchange may.
        let patience = match code {
            ERROR_FILE_NOT_FOUND => PIPE_RECONNECT_WINDOW,
            ERROR_PIPE_BUSY => PIPE_IO_TIMEOUT,
            _ => Duration::ZERO,
        };
        if started.elapsed() < patience {
            thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }
        crate::bail_code!(
            crate::error_codes::BRG_005,
            "{}",
            crate::i18n::ui_format!(
                "SSDownload çalışan örneğine bağlanılamadı (Windows hata kodu {code})",
                "Could not connect to the running SSDownload instance (Windows error code {code})",
                code = code
            )
        );
    }
}

/// The process owning the server end of a connected pipe, as reported by the kernel for that
/// endpoint. Never derived from request data.
fn pipe_server_process_id(pipe: HANDLE) -> Result<u32> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 || pid == 0 {
        return Err(win32_error(
            "Named pipe sunucu süreci belirlenemedi",
            "The named pipe server process could not be determined",
        ));
    }
    Ok(pid)
}

/// Refuses a pipe that another user's process serves. Any local user can create the pipe under
/// its predictable name before the application does, and a request carries URLs, headers and
/// session cookies, so the serving process is identified before the first write. A process
/// whose identity cannot be read is refused as well.
fn verify_pipe_server(pipe: HANDLE, owner_is_ours: OwnerCheck) -> Result<()> {
    match pipe_server_process_id(pipe).and_then(owner_is_ours) {
        Ok(true) => Ok(()),
        Ok(false) => crate::bail_code!(
            crate::error_codes::BRG_005,
            "{}",
            ui(
                "SSDownload yerel iletişim kanalını başka bir kullanıcının süreci sunuyor; hiçbir şey gönderilmedi",
                "The SSDownload local pipe is served by a process of another user; nothing was sent"
            )
        ),
        Err(error) => crate::bail_code!(
            crate::error_codes::BRG_005,
            "{}",
            crate::i18n::ui_format!("SSDownload yerel iletişim kanalını sunan sürecin sahibi doğrulanamadı; hiçbir şey gönderilmedi ({error:#})", "The owner of the process serving the SSDownload local pipe could not be verified; nothing was sent ({error:#})", error = error)
        ),
    }
}

/// The instance a second launch hands over to: the process that serves this profile's pipe,
/// or `None` while nothing listens on it. One profile's pipe belongs to one instance, so this
/// is what identifies that instance - and its window - among the profiles running side by
/// side. An instance that has not reached `start` yet listens nowhere and has no window
/// either, so `None` is also the answer to "is there a window to raise".
pub fn instance_process_id(paths: &AppPaths) -> Option<u32> {
    let pipe = connect_pipe(paths).ok()?;
    pipe_server_process_id(pipe.0).ok()
}

/// Lets the desktop instance behind the connected endpoint raise its own window once, so the
/// picker it is about to open is not denied activation while the browser stays in front. The
/// caller must be able to set the foreground window itself; the permission it passes on is
/// bound to that one process and is revoked by the next user input or by any other process
/// granting it, so it is transferred per handoff and never globally.
fn grant_foreground(pipe: HANDLE) -> Result<()> {
    let desktop_pid = pipe_server_process_id(pipe)?;
    if !same_user_process(desktop_pid)? {
        bail!(
            "{}",
            ui(
                "Named pipe sunucusu geçerli kullanıcıya ait değil",
                "The named pipe server does not belong to the current user"
            )
        );
    }
    if unsafe { AllowSetForegroundWindow(desktop_pid) } == 0 {
        return Err(win32_error(
            "Ön plan izni masaüstüne devredilemedi",
            "The foreground permission could not be passed to the desktop",
        ));
    }
    Ok(())
}

/// Switches a server pipe between blocking (waiting for a client) and non-blocking
/// (bounded reads of a connected client) mode; message read mode is kept in both.
fn set_pipe_wait_mode(pipe: HANDLE, wait: bool) {
    let mode = PIPE_READMODE_MESSAGE | if wait { PIPE_WAIT } else { PIPE_NOWAIT };
    unsafe {
        SetNamedPipeHandleState(pipe, &mode, ptr::null(), ptr::null());
    }
}

fn process_pipe_client(app: &App, pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<()> {
    // Impersonation uses the identity of the last message successfully read.
    let input = wait_for_message(pipe, PIPE_IO_TIMEOUT)?;
    if !same_user_client(pipe)? {
        bail!(
            "{}",
            ui(
                "Named pipe istemcisi geçerli kullanıcıya ait değil",
                "The named pipe client does not belong to the current user"
            )
        );
    }
    let mut response_offset = 0;
    let response = match serde_json::from_slice::<WireRequest>(&input) {
        Ok(request) if request.version != PROTOCOL_VERSION => BridgeResponse {
            result: None,
            ok: false,
            message: unsupported_version(request.version),
            snapshot: None,
        },
        Ok(request) => match request.action {
            Some(action) => {
                let media_only = matches!(
                    action,
                    Action::Inspect { .. } | Action::InspectStatus { .. }
                );
                let page = match &action {
                    Action::StatusPage { offset, limit } => (*offset, *limit),
                    _ => (0, 100),
                };
                response_offset = page.0;
                let include_snapshot = media_only
                    || matches!(
                        action,
                        Action::Status
                            | Action::StatusPage { .. }
                            | Action::ToolsStatus { .. }
                            | Action::InstallTools { .. }
                    );
                match app.dispatch_result(action) {
                    Ok(result) => BridgeResponse {
                        result: Some(result),
                        ok: true,
                        message: ui("İstek kabul edildi", "Request accepted").into(),
                        snapshot: include_snapshot.then(|| {
                            if media_only {
                                redact_snapshot(app.snapshot_page(0, 0))
                            } else {
                                public_snapshot(app.snapshot_page(page.0, page.1))
                            }
                        }),
                    },
                    Err(error) => BridgeResponse {
                        result: None,
                        ok: false,
                        message: error.to_string(),
                        snapshot: None,
                    },
                }
            }
            None => BridgeResponse {
                result: None,
                ok: true,
                message: ui("SSDownload çalışıyor", "SSDownload is running").into(),
                snapshot: None,
            },
        },
        Err(error) => BridgeResponse {
            result: None,
            ok: false,
            message: crate::i18n::ui_format!(
                "Geçersiz yerel iletişim iletisi: {error}",
                "Invalid local message: {error}",
                error = error
            ),
            snapshot: None,
        },
    };
    let mut response = response;
    let mut output = serde_json::to_vec(&response)?;
    while output.len() > MAX_PIPE_MESSAGE {
        if let Some(snapshot) = &mut response.snapshot {
            if !snapshot.jobs.is_empty() {
                let keep = snapshot.jobs.len() / 2;
                std::sync::Arc::make_mut(&mut snapshot.jobs).truncate(keep);
                snapshot.next_offset = Some(response_offset + keep);
                if keep == 0 {
                    response.ok = false;
                    response.message = ui(
                        "Tek iş izin verilen ileti boyutunu aşıyor",
                        "A single job exceeds the allowed message size",
                    )
                    .into();
                    response.snapshot = None;
                    output = serde_json::to_vec(&response)?;
                    continue;
                }
                response.message = ui(
                    "Durum özeti son işleri gösteriyor; tüm geçmiş masaüstünde mevcut.",
                    "The status summary shows the latest jobs; the full history is available on the desktop.",
                )
                .into();
            } else if let Some(media) = &mut snapshot.media {
                if media.formats.len() > 1 {
                    media.formats.truncate(media.formats.len() / 2);
                } else {
                    response.snapshot = None;
                }
            } else {
                response.snapshot = None;
            }
        } else {
            break;
        }
        output = serde_json::to_vec(&response)?;
    }
    write_message(pipe, &output)
}

/// Starts the per-user, local-only named-pipe server and returns once its listener is ready.
pub fn start(app: App) -> Result<()> {
    let name = app.paths().pipe_name();
    let security = pipe_security()?;
    // Establish all listeners before returning. The first handle reserves the
    // name exclusively; subsequent handles retain the same per-user ACL.
    // Reuse instances rather than leaving a name-ownership gap on disconnect.
    let mut pipes = Vec::with_capacity(PIPE_WORKERS as usize);
    for index in 0..PIPE_WORKERS {
        pipes.push(create_pipe(&name, &security, index == 0)?);
    }
    for (index, pipe) in pipes.into_iter().enumerate() {
        let app = app.clone();
        thread::Builder::new()
            .name(format!("ssdownload-bridge-{index}"))
            .spawn(move || {
                let pipe = pipe;
                let mut last_failure_log: Option<Instant> = None;
                loop {
                    if app.quit_requested() {
                        break;
                    }
                    // Waiting for a client blocks in the kernel instead of polling; the
                    // connected exchange then switches to non-blocking reads so a silent
                    // client is bounded by `PIPE_IO_TIMEOUT`.
                    set_pipe_wait_mode(pipe.0, true);
                    let connected = if unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) } != 0 {
                        true
                    } else {
                        match unsafe { GetLastError() } {
                            ERROR_PIPE_CONNECTED => true,
                            ERROR_NO_DATA => {
                                unsafe {
                                    DisconnectNamedPipe(pipe.0);
                                }
                                false
                            }
                            code => {
                                // A listener never exits on an unexpected error: it records
                                // the failure (at most once a minute) and keeps serving.
                                if last_failure_log
                                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
                                {
                                    last_failure_log = Some(Instant::now());
                                    crate::logging::record(
                                        crate::logging::Event::warn("bridge.listen_failed")
                                            .outcome(crate::logging::Outcome::Failed)
                                            .detail(format!("worker={index} windows={code}")),
                                    );
                                }
                                unsafe {
                                    DisconnectNamedPipe(pipe.0);
                                }
                                thread::sleep(Duration::from_millis(500));
                                false
                            }
                        }
                    };
                    if !connected {
                        continue;
                    }
                    if app.quit_requested() {
                        unsafe {
                            DisconnectNamedPipe(pipe.0);
                        }
                        break;
                    }
                    set_pipe_wait_mode(pipe.0, false);
                    let _ = process_pipe_client(&app, pipe.0);
                    unsafe {
                        FlushFileBuffers(pipe.0);
                        DisconnectNamedPipe(pipe.0);
                    }
                }
            })
            .context(ui(
                "Yerel iletişim iş parçacığı başlatılamadı",
                "The local communication thread could not be started",
            ))?;
    }
    Ok(())
}

fn transact(paths: &AppPaths, action: Option<&Action>) -> Result<BridgeResponse> {
    transact_checked(paths, action, same_user_process)
}

/// `transact` with the identity check as a parameter (see `connect_pipe_checked`).
fn transact_checked(
    paths: &AppPaths,
    action: Option<&Action>,
    owner_is_ours: OwnerCheck,
) -> Result<BridgeResponse> {
    let request = WireRequest {
        version: PROTOCOL_VERSION,
        action: action.cloned(),
    };
    let input = serde_json::to_vec(&request)?;
    if input.len() > MAX_PIPE_MESSAGE {
        crate::bail_code!(crate::error_codes::BRG_002);
    }
    // The connection is verified before the request is written and is never reopened. The
    // exchange has no read timeout of its own: a busy desktop answers when it can.
    let pipe = connect_pipe_checked(paths, owner_is_ours)?;
    let mut output = vec![0u8; MAX_PIPE_MESSAGE];
    let mut read = 0u32;
    if unsafe {
        TransactNamedPipe(
            pipe.0,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut read,
            ptr::null_mut(),
        )
    } == 0
    {
        let code = unsafe { GetLastError() };
        crate::bail_code!(
            crate::error_codes::BRG_005,
            "{}",
            crate::i18n::ui_format!(
                "SSDownload çalışan örneğine bağlanılamadı (Windows hata kodu {code})",
                "Could not connect to the running SSDownload instance (Windows error code {code})",
                code = code
            )
        );
    }
    output.truncate(read as usize);
    serde_json::from_slice(&output).context(ui(
        "SSDownload yerel iletişim yanıtı geçersiz",
        "The SSDownload local communication reply is invalid",
    ))
}

/// Sends any local application action to the already-running per-user instance.
pub fn send(paths: &AppPaths, action: &Action) -> Result<BridgeResponse> {
    transact(paths, Some(action))
}

/// Sends a validated media handoff over a connection this process holds open. Holding the
/// connection lets the desktop instance behind that endpoint be granted foreground permission
/// before the handoff is written, so the permission is already in place when the desktop
/// handles the request and opens its picker. The handoff is written exactly once, and a
/// refused grant is only recorded: the download stays valid and just falls back to the
/// desktop's own activation.
fn send_browser_media(
    paths: &AppPaths,
    action: &Action,
    log: &mut crate::logging::HostLog,
) -> Result<BridgeResponse> {
    let request = WireRequest {
        version: PROTOCOL_VERSION,
        action: Some(action.clone()),
    };
    let input = serde_json::to_vec(&request)?;
    if input.len() > MAX_PIPE_MESSAGE {
        crate::bail_code!(crate::error_codes::BRG_002);
    }
    // Readiness probes carry no action, so they may be retried; the handoff is written once,
    // on the connection that carries the grant, after the desktop is listening.
    ensure_desktop_ready(paths)?;
    let pipe = connect_pipe(paths)?;
    if let Err(error) = grant_foreground(pipe.0) {
        log.record(
            crate::logging::Event::warn("bridge.foreground_grant_failed")
                .outcome(crate::logging::Outcome::Failed)
                .detail(format!("{error:#}")),
        );
    }
    write_message(pipe.0, &input)?;
    // Nothing is retried from here on: the desktop may already hold the handoff.
    let output = wait_for_message(pipe.0, PIPE_IO_TIMEOUT)?;
    serde_json::from_slice(&output).context(ui(
        "SSDownload yerel iletişim yanıtı geçersiz",
        "The SSDownload local communication reply is invalid",
    ))
}

fn redact_snapshot(snapshot: AppSnapshot) -> AppSnapshot {
    let snapshot = public_snapshot(snapshot);
    AppSnapshot {
        jobs: std::sync::Arc::new(Vec::new()),
        revision: snapshot.revision,
        warning: snapshot.warning,
        next_offset: None,
        total_jobs: 0,
        tools_id: None,
        tools_error: None,
        settings: Settings::default(),
        completion_countdown: None,
        completion_events: Vec::new(),
        media: snapshot.media,
        // A browser response never carries the handoff: it holds headers,
        // cookies and the exact selection the user has not confirmed yet.
        media_launch: None,
        inspecting: snapshot.inspecting,
        inspect_id: snapshot.inspect_id,
        inspect_generation: snapshot.inspect_generation,
        inspect_error: snapshot.inspect_error,
        inspect_error_code: snapshot.inspect_error_code,
        tools: Vec::new(),
        installing_tools: false,
        tool_progress: None,
        messages: Vec::new(),
        show_window_seq: 0,
        quit_requested: false,
    }
}

fn public_snapshot(mut snapshot: AppSnapshot) -> AppSnapshot {
    snapshot.media_launch = None;
    if let Some(media) = &mut snapshot.media {
        if let Ok(url) = url::Url::parse(&media.webpage_url) {
            media.webpage_url = url.origin().ascii_serialization();
        }
        media.thumbnail = None;
    }
    for job in std::sync::Arc::make_mut(&mut snapshot.jobs) {
        job.request.headers.clear();
        job.request.session_cookies.clear();
        job.request.external_subtitles.clear();
        job.request.source_identity = None;
        for value in [&mut job.request.url, &mut job.name] {
            if let Ok(mut url) = url::Url::parse(value) {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                *value = url.into();
            }
        }
        job.request.page_url = None;
        job.request.referer = None;
        // Page markup is report material: a browser response echoes nothing back.
        job.request.page_html = None;
    }
    snapshot
}

fn browser_response(mut response: BridgeResponse, include_media: bool) -> BridgeResponse {
    response.snapshot = if include_media {
        response.snapshot.map(redact_snapshot)
    } else {
        None
    };
    response
}

fn validate_http_url(value: &str) -> Result<()> {
    let parsed = url::Url::parse(value).context(ui("URL geçersiz", "The URL is invalid"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        crate::bail_code!(crate::error_codes::MED_005);
    }
    if parsed.host_str().is_none() {
        bail!(
            "{}",
            ui(
                "URL bir sunucu adı içermiyor",
                "The URL does not contain a host name"
            )
        );
    }
    Ok(())
}

fn validate_browser_headers(headers: &std::collections::BTreeMap<String, String>) -> Result<()> {
    if headers.len() > 16
        || headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>()
            > 64 * 1024
    {
        bail!(
            "{}",
            ui(
                "Oturum üstbilgileri çok büyük",
                "The session headers are too large"
            )
        );
    }
    const ALLOWED_HEADERS: &[&str] = &["user-agent", "accept", "accept-language", "origin"];
    if headers
        .keys()
        .any(|name| !ALLOWED_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
    {
        crate::bail_code!(crate::error_codes::BRG_004);
    }
    Ok(())
}

fn validate_browser_add(request: &mut crate::model::AddRequest) -> Result<()> {
    validate_http_url(&request.url)?;
    if let Some(referer) = &request.referer {
        validate_http_url(referer)?;
    }
    validate_browser_headers(&request.headers)?;
    for subtitle in &request.external_subtitles {
        validate_http_url(&subtitle.url)?;
        if let Some(referer) = &subtitle.referer {
            validate_http_url(referer)?;
        }
        validate_browser_headers(&subtitle.headers)?;
    }
    // The extension exports at most 500 cookies, each within the browser's own size limits.
    if request.session_cookies.len() > MAX_BROWSER_COOKIES
        || request.session_cookies.iter().any(|cookie| {
            cookie.name.len() > 1024
                || cookie.value.len() > 8192
                || cookie.domain.len() > 255
                || cookie.path.len() > 2048
        })
    {
        bail!(
            "{}",
            ui(
                "Oturum çerezleri sınırı aşıyor",
                "The session cookies exceed the limit"
            )
        );
    }
    if request
        .filename
        .as_ref()
        .is_some_and(|name| name.len() > MAX_BROWSER_FILENAME)
    {
        bail!("{}", ui("Dosya adı çok uzun", "The file name is too long"));
    }
    // Page markup is report material, not a transfer input: what the extension
    // hands over stays inside this bound so a report (or any later reader) can
    // never be handed an unbounded page.
    if request
        .page_html
        .as_ref()
        .is_some_and(|html| html.len() > MAX_PAGE_HTML)
    {
        bail!(
            "{}",
            ui(
                "Sayfa işaretlemesi çok büyük",
                "The page markup is too large"
            )
        );
    }
    // A browser extension cannot select arbitrary local filesystem destinations.
    request.directory = None;
    Ok(())
}

fn validate_browser_action(action: &mut Action) -> Result<()> {
    match action {
        Action::Add { request } => validate_browser_add(request)?,
        Action::CompleteSourceRefresh {
            id, token, request, ..
        } => {
            uuid::Uuid::parse_str(id)
                .context(ui("İş kimliği geçersiz", "The job ID is invalid"))?;
            if token.is_empty() || token.len() > 128 {
                bail!(
                    "{}",
                    ui(
                        "Kaynak yenileme yetkisi geçersiz",
                        "The source refresh authorization is invalid"
                    )
                );
            }
            validate_browser_add(request)?;
        }
        Action::BeginSourceRefresh { id } => {
            uuid::Uuid::parse_str(id)
                .context(ui("İş kimliği geçersiz", "The job ID is invalid"))?;
        }
        Action::Capabilities => {}
        // Extension events are bounded relay data, not an application command; the
        // application validates every field again before it records anything.
        Action::Log { request } => crate::validation::validate_relay_events(&request.events)?,
        Action::BrowserTransfer {
            command:
                crate::browser_transfer::Command::Begin {
                    sender_pid,
                    sender_started_at,
                    ..
                },
        } if sender_pid.is_some() || sender_started_at.is_some() => {
            bail!(
                "{}",
                ui(
                    "Tarayıcı aktarım gönderici kimliği eklentiye açık değil",
                    "The browser transfer sender identity is not exposed to the extension"
                )
            )
        }
        Action::BrowserTransfer {
            command: crate::browser_transfer::Command::Release { .. },
        } => bail!(
            "{}",
            ui(
                "Tarayıcı aktarım gönderici serbest bırakma komutu eklentiye açık değil",
                "The browser transfer sender release command is not exposed to the extension"
            )
        ),
        Action::BrowserTransfer { command } => command.validate_frame()?,
        Action::Inspect { request } => {
            validate_http_url(&request.url)?;
            if let Some(referer) = &request.referer {
                validate_http_url(referer)?;
            }
            validate_browser_headers(&request.headers)?;
        }
        Action::BrowserMedia {
            launch_id, request, ..
        } => {
            if launch_id.is_empty() || launch_id.len() > 128 {
                bail!("{}", ui("Geçersiz istek kimliği", "Invalid request ID"));
            }
            validate_browser_add(request)?;
            // The native picker owns the queued job's idempotency key: an
            // extension-supplied one would collide with the differently
            // configured add the user finally confirms.
            request.request_id = None;
        }
        Action::ShowWindow => {}
        Action::InspectStatus { request_id }
            if !request_id.is_empty() && request_id.len() <= 128 => {}
        _ => crate::bail_code!(
            crate::error_codes::BRG_004,
            "{}",
            ui(
                "Bu işlem tarayıcı eklentisine açık değil",
                "This operation is not available to the browser extension"
            )
        ),
    }
    Ok(())
}

/// Recognizes explicit CLI mode and the argument shape Chrome uses when it starts the
/// registered native host from its manifest executable path.
pub fn is_native_invocation(args: &[String]) -> bool {
    args.iter().any(|argument| {
        argument == "--native-host"
            || argument
                .to_ascii_lowercase()
                .starts_with("chrome-extension://")
    })
}

/// Origin strings a Chromium browser may pass to the native host, one per allowed ID.
fn allowed_extension_origins() -> Vec<String> {
    crate::extension::ALLOWED_EXTENSION_IDS
        .iter()
        .map(|id| format!("chrome-extension://{id}"))
        .collect()
}

fn launch_origin_allowed(arguments: &[String]) -> bool {
    for argument in arguments {
        let lower = argument.to_ascii_lowercase();
        if lower.starts_with("chrome-extension://") {
            let origin = lower.trim_end_matches('/');
            if !allowed_extension_origins()
                .iter()
                .any(|allowed| allowed == origin)
            {
                return false;
            }
        } else if lower.contains("-extension://")
            || (argument.contains('@') && !argument.contains('\\') && !argument.contains('/'))
        {
            return false;
        }
    }
    // If Chrome supplies an identity, it must match the registered extension. The
    // manifest's allowed_origins remains the browser-enforced boundary when none is supplied.
    true
}

fn spawn_background(paths: &AppPaths) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let executable = std::env::current_exe().context(ui(
        "SSDownload çalıştırılabilir dosyası bulunamadı",
        "The SSDownload executable was not found",
    ))?;
    Command::new(executable)
        .arg("--background")
        .arg("--data-dir")
        .arg(&paths.base_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context(ui(
            "SSDownload arka planda başlatılamadı",
            "SSDownload could not be started in the background",
        ))?;
    Ok(())
}

/// Runs the side-effect-free readiness handshake every action shares, starting the desktop
/// instance when nothing answers yet. Retries here only transmit requests without an action,
/// so a handoff sent through `send_browser_media` is still written at most once.
fn ensure_desktop_ready(paths: &AppPaths) -> Result<BridgeResponse> {
    match transact(paths, None) {
        Ok(response) => Ok(response),
        Err(first_error) => {
            spawn_background(paths)?;
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                match transact(paths, None) {
                    Ok(response) => return Ok(response),
                    Err(_) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(100))
                    }
                    Err(_) => {
                        return Err(anyhow!(
                            "{}",
                            crate::i18n::ui_format!("SSDownload başlatıldı ancak yerel iletişim hazır olmadı: {first_error}", "SSDownload started but local communication did not become ready: {first_error}", first_error = first_error)
                        ))
                    }
                }
            }
        }
    }
}

fn transact_with_start(paths: &AppPaths, action: Option<&Action>) -> Result<BridgeResponse> {
    let readiness = ensure_desktop_ready(paths)?;
    // Readiness retries are side-effect free. The requested action is transmitted exactly once.
    match action {
        Some(action) => transact(paths, Some(action)),
        None => Ok(readiness),
    }
}

fn read_native_message(input: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    let mut offset = 0;
    while offset < length.len() {
        match input.read(&mut length[offset..]) {
            Ok(0) if offset == 0 => return Ok(None),
            Ok(0) => crate::bail_code!(
                crate::error_codes::BRG_002,
                "{}",
                ui(
                    "Native Messaging uzunluk başlığı yarım kaldı",
                    "The Native Messaging length header was cut off"
                )
            ),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(error).context(ui(
                    "Native Messaging girdisi okunamadı",
                    "The Native Messaging input could not be read",
                ))
            }
        }
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_NATIVE_MESSAGE {
        crate::bail_code!(
            crate::error_codes::BRG_002,
            "{}",
            crate::i18n::ui_format!(
                "Native Messaging iletisi geçersiz boyutta: {length}",
                "The Native Messaging message has an invalid size: {length}",
                length = length
            )
        );
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body).context(ui(
        "Native Messaging iletisi yarım kaldı",
        "The Native Messaging message was cut off",
    ))?;
    Ok(Some(body))
}

fn write_native_message(output: &mut impl Write, response: &BridgeResponse) -> Result<()> {
    let body = serde_json::to_vec(response)?;
    if body.len() > MAX_NATIVE_MESSAGE {
        crate::bail_code!(crate::error_codes::BRG_002);
    }
    output.write_all(&(body.len() as u32).to_le_bytes())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

// Native host termination is not evidence that Chrome stopped an in-flight fetch.
// The extension must abort its readers and explicitly end every granted request after
// reconnecting; releasing transfer ownership here could oversubscribe the shared budget.
fn process_started_at(handle: HANDLE) -> Result<u64> {
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut created = zero();
    let mut exited = zero();
    let mut kernel = zero();
    let mut user = zero();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error()).context(ui(
            "Native Messaging gönderici başlangıcı okunamadı",
            "The Native Messaging sender start time could not be read",
        ));
    }
    Ok(u64::from(created.dwLowDateTime) | (u64::from(created.dwHighDateTime) << 32))
}

fn bind_native_transfer_sender(action: &mut Action) -> Result<()> {
    let Action::BrowserTransfer {
        command:
            crate::browser_transfer::Command::Begin {
                sender_pid,
                sender_started_at,
                ..
            },
    } = action
    else {
        return Ok(());
    };
    let started_at = process_started_at(unsafe { GetCurrentProcess() })?;
    *sender_pid = Some(std::process::id());
    *sender_started_at = Some(started_at);
    Ok(())
}

/// Records a refused browser action with the code the validation carried, so a failing
/// extension request is visible in the log even though the reply is just text.
fn record_browser_action_failure(
    log: &mut crate::logging::HostLog,
    action: &Action,
    error: &anyhow::Error,
) {
    let name = match action {
        Action::Add { .. } => "bridge.browser_add_rejected",
        Action::Inspect { .. } => "bridge.browser_inspect_rejected",
        Action::BrowserMedia { .. } => "bridge.browser_media_rejected",
        Action::BrowserTransfer { .. } => "bridge.browser_transfer_rejected",
        Action::CompleteSourceRefresh { .. } | Action::BeginSourceRefresh { .. } => {
            "bridge.browser_refresh_rejected"
        }
        _ => "bridge.browser_action_rejected",
    };
    match crate::error_codes::code_of_text(error) {
        Some(code) => log.record(crate::logging::Event::failure(
            name,
            code,
            format!("{error:#}"),
        )),
        None => log.record(
            crate::logging::Event::warn(name)
                .outcome(crate::logging::Outcome::Failed)
                .detail(format!("{error:#}")),
        ),
    }
}

/// Whether a browser action may start the desktop when nothing answers. The extension's
/// background probes (capabilities at every page load, session-log batches) are passive: a
/// desktop the user quit must stay closed, and those probes report "not running" instead.
/// Only an explicit user action (picker, add, show window, transfer) starts it.
fn starts_desktop(action: &Action) -> bool {
    !matches!(action, Action::Capabilities | Action::Log { .. })
}

/// Runs the browser Native Messaging stdio loop. Multiple framed messages are supported.
pub fn native_host(paths: &AppPaths) -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_native_host(paths, &arguments, &mut stdin.lock(), &mut stdout.lock())
}

fn serve_native_host(
    paths: &AppPaths,
    arguments: &[String],
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<()> {
    let mut log = crate::logging::HostLog::new(paths);
    if !launch_origin_allowed(arguments) {
        log.record(crate::logging::Event::failure(
            "bridge.identity_rejected",
            crate::error_codes::BRG_001,
            ui(
                "izin verilmeyen eklenti kaynağı",
                "disallowed extension origin",
            ),
        ));
        crate::bail_code!(crate::error_codes::BRG_001);
    }
    loop {
        let body = match read_native_message(input) {
            Ok(Some(body)) => body,
            Ok(None) => break,
            Err(error) => {
                let response = BridgeResponse {
                    result: None,
                    ok: false,
                    message: error.to_string(),
                    snapshot: None,
                };
                write_native_message(output, &response)?;
                break;
            }
        };
        let response = match serde_json::from_slice::<WireRequest>(&body) {
            Err(error) => BridgeResponse {
                result: None,
                ok: false,
                message: crate::i18n::ui_format!(
                    "Geçersiz Native Messaging iletisi: {error}",
                    "Invalid Native Messaging message: {error}",
                    error = error
                ),
                snapshot: None,
            },
            Ok(request) if request.version != PROTOCOL_VERSION => {
                log.record(crate::logging::Event::failure(
                    "bridge.protocol_mismatch",
                    crate::error_codes::BRG_006,
                    crate::i18n::ui_format!(
                        "istemci sürümü {0}",
                        "client version {0}",
                        request.version
                    ),
                ));
                BridgeResponse {
                    result: None,
                    ok: false,
                    message: unsupported_version(request.version),
                    snapshot: None,
                }
            }
            Ok(mut request) => match request.action.as_mut() {
                None => match transact_with_start(paths, None) {
                    Ok(response) => browser_response(response, true),
                    Err(error) => BridgeResponse {
                        result: None,
                        ok: false,
                        message: error.to_string(),
                        snapshot: None,
                    },
                },
                Some(action) => match validate_browser_action(action) {
                    Err(error) => {
                        record_browser_action_failure(&mut log, action, &error);
                        BridgeResponse {
                            result: None,
                            ok: false,
                            message: error.to_string(),
                            snapshot: None,
                        }
                    }
                    Ok(()) => match bind_native_transfer_sender(action) {
                        Err(error) => BridgeResponse {
                            result: None,
                            ok: false,
                            message: error.to_string(),
                            snapshot: None,
                        },
                        Ok(()) => {
                            let include_media = matches!(
                                action,
                                Action::Inspect { .. } | Action::InspectStatus { .. }
                            );
                            // The picker handoff keeps a handle to the desktop it reaches so
                            // the foreground permission is transferred before it arrives;
                            // every other action stays on the plain transaction.
                            let dispatched = if matches!(action, Action::BrowserMedia { .. }) {
                                send_browser_media(paths, action, &mut log)
                            } else if starts_desktop(action) {
                                transact_with_start(paths, Some(action))
                            } else {
                                transact(paths, Some(action))
                            };
                            match dispatched {
                                Ok(response) => browser_response(response, include_media),
                                Err(error) => BridgeResponse {
                                    result: None,
                                    ok: false,
                                    message: error.to_string(),
                                    snapshot: None,
                                },
                            }
                        }
                    },
                },
            },
        };
        write_native_message(output, &response)?;
    }
    Ok(())
}

fn manifest_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("SSDownload")
        .join("NativeMessaging")
}

fn registry_set_default(key_path: &str, value: &Path) -> Result<()> {
    let key_path = wide(key_path);
    let mut key: HKEY = ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Native host kayıt defteri anahtarı oluşturulamadı (Windows hata kodu {status})",
                "The native host registry key could not be created (Windows error code {status})",
                status = status
            )
        );
    }
    let value = wide(&value.to_string_lossy());
    let status = unsafe {
        RegSetValueExW(
            key,
            ptr::null(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * std::mem::size_of::<u16>()) as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if status != 0 {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Native host kayıt defteri değeri yazılamadı (Windows hata kodu {status})",
                "The native host registry value could not be written (Windows error code {status})",
                status = status
            )
        );
    }
    Ok(())
}

fn registry_delete_tree(key_path: &str) -> Result<()> {
    let key_path = wide(key_path);
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_path.as_ptr()) };
    if status == 0 || status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Native host kayıt defteri anahtarı silinemedi (Windows hata kodu {status})",
                "The native host registry key could not be deleted (Windows error code {status})",
                status = status
            )
        )
    }
}

fn registry_locations() -> [&'static str; 3] {
    [
        r"Software\Google\Chrome\NativeMessagingHosts\com.ssdownload.desktop",
        r"Software\Chromium\NativeMessagingHosts\com.ssdownload.desktop",
        r"Software\Microsoft\Edge\NativeMessagingHosts\com.ssdownload.desktop",
    ]
}

/// Writes per-user Native Messaging manifests and registers them for supported browsers.
pub fn register(executable: &Path, paths: &AppPaths) -> Result<()> {
    std::fs::create_dir_all(manifest_root())?;
    let _lock = crate::output::OutputLock::acquire(&manifest_root().join("registration"))?;
    let owned_root = installation_root(executable)?;
    let executable = executable.canonicalize().with_context(|| {
        crate::i18n::ui_format!(
            "Çalıştırılabilir dosya bulunamadı: {0}",
            "The executable was not found: {0}",
            executable.display()
        )
    })?;
    // Browser launchers do not reliably accept Rust's extended-length Windows prefix.
    let executable = executable.to_str().context(ui(
        "Native host yolu geçerli Unicode değil",
        "The native host path is not valid Unicode",
    ))?;
    let executable = if let Some(path) = executable.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        executable
            .strip_prefix(r"\\?\")
            .unwrap_or(executable)
            .to_string()
    };
    let root = owned_root;
    std::fs::create_dir_all(&root).context(ui(
        "Native Messaging klasörü oluşturulamadı",
        "The Native Messaging folder could not be created",
    ))?;
    let chromium_manifest = root.join(format!("{HOST_NAME}.chromium.json"));
    let chrome = serde_json::json!({
        "name": HOST_NAME,
        "description": "SSDownload Chrome integration",
        "path": executable,
        "type": "stdio",
        "allowed_origins": allowed_extension_origins()
            .into_iter()
            .map(|origin| format!("{origin}/"))
            .collect::<Vec<_>>()
    });
    crate::recovery::atomic_write(&chromium_manifest, &serde_json::to_vec_pretty(&chrome)?)
        .context(ui(
            "Chrome Native Messaging manifesti yazılamadı",
            "The Chrome Native Messaging manifest could not be written",
        ))?;

    crate::recovery::atomic_write(
        &root.join("profile.json"),
        &serde_json::to_vec(&paths.base_dir)?,
    )?;
    for location in registry_locations() {
        registry_set_default(location, &chromium_manifest)?;
    }
    Ok(())
}

/// Removes all per-user Native Messaging registrations and generated manifests.
pub fn unregister(executable: &Path, paths: &AppPaths) -> Result<()> {
    std::fs::create_dir_all(manifest_root())?;
    let _lock = crate::output::OutputLock::acquire(&manifest_root().join("registration"))?;
    let root = installation_root(executable)?;
    if registered_profile(executable)?.is_some_and(|p| p != paths.base_dir) {
        bail!(
            "{}",
            ui(
                "Bu kurulum farklı bir veri profili için kayıtlı; diğer profilin kaydı korunuyor",
                "This installation is registered for a different data profile; the other profile's registration is kept"
            )
        )
    }
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let chromium_manifest = root.join(format!("{HOST_NAME}.chromium.json"));
    for location in registry_locations() {
        let Ok(key) = hkcu.open_subkey(location) else {
            continue;
        };
        let actual: String = key.get_value("")?;
        if Path::new(&actual) == chromium_manifest.as_path() {
            registry_delete_tree(location)?;
        }
    }
    for name in [format!("{HOST_NAME}.chromium.json"), "profile.json".into()] {
        let path = root.join(name);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}
fn installation_root(executable: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let path = executable.canonicalize()?;
    let id = hex::encode(Sha256::digest(
        path.to_string_lossy().to_lowercase().as_bytes(),
    ));
    Ok(manifest_root().join(id))
}
pub fn registered_profile(executable: &Path) -> Result<Option<PathBuf>> {
    let path = installation_root(executable)?.join("profile.json");
    if !path.exists() {
        return Ok(None);
    }
    let profile: PathBuf = serde_json::from_slice(&std::fs::read(path)?)?;
    if !profile.is_absolute() {
        bail!(
            "{}",
            ui(
                "Native host profil yolu geçersiz",
                "The native host profile path is invalid"
            )
        )
    }
    Ok(Some(profile))
}

/// True while any SSDownload copy owns a browser Native Messaging registration.
pub fn host_registered() -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    registry_locations()
        .iter()
        .any(|location| hkcu.open_subkey(location).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Action, AddRequest, BridgeResponse, ExternalSubtitle, InspectRequest, ScopedCookie,
    };

    #[test]
    fn browser_handoffs_accept_bounded_page_markup_and_refuse_a_larger_one() {
        let mut request = AddRequest {
            url: "https://example.test/clip".into(),
            ..AddRequest::default()
        };
        request.page_html = Some("x".repeat(MAX_PAGE_HTML));
        assert!(validate_browser_add(&mut request).is_ok());
        // A handoff is not a transfer input: whatever carries markup stays
        // inside the bound the report and the snapshots rely on.
        request.page_html = Some("x".repeat(MAX_PAGE_HTML + 1));
        assert!(validate_browser_add(&mut request).is_err());
        request.page_html = None;
        assert!(validate_browser_add(&mut request).is_ok());
    }

    #[test]
    fn native_messages_preserve_framing_across_short_reads() {
        struct ShortReader {
            bytes: Vec<u8>,
            position: usize,
        }
        impl Read for ShortReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.position == self.bytes.len() {
                    return Ok(0);
                }
                let count = 1.min(output.len()).min(self.bytes.len() - self.position);
                output[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
                self.position += count;
                Ok(count)
            }
        }

        let body = br#"{\"version\":1,\"action\":null}"#.to_vec();
        let mut bytes = (body.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&body);
        let mut reader = ShortReader { bytes, position: 0 };
        assert_eq!(read_native_message(&mut reader).unwrap(), Some(body));
        assert_eq!(read_native_message(&mut reader).unwrap(), None);
    }

    #[test]
    fn native_messages_reject_empty_truncated_and_oversized_frames() {
        assert!(read_native_message(&mut &b"\x00\x00\x00\x00"[..]).is_err());
        assert!(read_native_message(&mut &b"\x02\x00\x00\x00{"[..]).is_err());
        let oversized = ((MAX_NATIVE_MESSAGE as u32) + 1).to_le_bytes();
        assert!(read_native_message(&mut &oversized[..]).is_err());
    }

    #[test]
    fn native_message_writer_round_trips_response() {
        let response = BridgeResponse {
            result: None,
            ok: true,
            message: "tamam".into(),
            snapshot: None,
        };
        let mut bytes = Vec::new();
        write_native_message(&mut bytes, &response).unwrap();
        let body = read_native_message(&mut &bytes[..]).unwrap().unwrap();
        let decoded: BridgeResponse = serde_json::from_slice(&body).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.message, "tamam");
    }

    #[test]
    fn browser_actions_are_restricted_and_sanitized() {
        let mut add = Action::Add {
            request: AddRequest {
                url: "https://example.test/video".into(),
                directory: Some(PathBuf::from(r"C:\\not-allowed")),
                headers: [("User-Agent".into(), "SSDownload-test".into())].into(),
                session_cookies: vec![ScopedCookie {
                    name: "session".into(),
                    value: "value".into(),
                    domain: "example.test".into(),
                    path: "/".into(),
                    secure: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        validate_browser_action(&mut add).unwrap();
        match add {
            Action::Add { request } => {
                assert!(request.directory.is_none());
                assert_eq!(request.session_cookies.len(), 1);
            }
            _ => unreachable!(),
        }

        let mut forbidden_header = Action::Inspect {
            request: InspectRequest {
                url: "https://example.test".into(),
                headers: [("X-Forwarded-For".into(), "127.0.0.1".into())].into(),
                ..Default::default()
            },
        };
        assert!(validate_browser_action(&mut forbidden_header).is_err());

        let mut local_url = Action::Inspect {
            request: InspectRequest {
                url: "file:///C:/secret.txt".into(),
                ..Default::default()
            },
        };
        assert!(validate_browser_action(&mut local_url).is_err());
    }

    #[test]
    fn browser_actions_reject_raw_credential_headers_case_insensitively() {
        let mut add = Action::Add {
            request: AddRequest {
                url: "https://example.test/video".into(),
                headers: [("cOoKiE".into(), "value".into())].into(),
                ..Default::default()
            },
        };
        assert!(validate_browser_action(&mut add).is_err());

        let mut subtitle_add = Action::Add {
            request: AddRequest {
                url: "https://example.test/video".into(),
                external_subtitles: vec![ExternalSubtitle {
                    url: "https://example.test/captions".into(),
                    headers: [("Authorization".into(), "value".into())].into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        assert!(validate_browser_action(&mut subtitle_add).is_err());

        let mut refresh = Action::CompleteSourceRefresh {
            id: "c743e94c-1d09-4191-bcca-f111340bc3c4".into(),
            token: "refresh-token".into(),
            request: Box::new(AddRequest {
                url: "https://example.test/video".into(),
                headers: [("AUTHORIZATION".into(), "value".into())].into(),
                ..Default::default()
            }),
            restart: false,
        };
        assert!(validate_browser_action(&mut refresh).is_err());

        let mut inspect = Action::Inspect {
            request: InspectRequest {
                url: "https://example.test/video".into(),
                headers: [("Cookie".into(), "value".into())].into(),
                ..Default::default()
            },
        };
        assert!(validate_browser_action(&mut inspect).is_err());
    }

    #[test]
    fn native_registration_covers_chrome_chromium_and_edge() {
        assert_eq!(
            registry_locations(),
            [
                r"Software\Google\Chrome\NativeMessagingHosts\com.ssdownload.desktop",
                r"Software\Chromium\NativeMessagingHosts\com.ssdownload.desktop",
                r"Software\Microsoft\Edge\NativeMessagingHosts\com.ssdownload.desktop",
            ]
        );
        assert!(is_native_invocation(&[format!(
            "{}/",
            allowed_extension_origins()[0]
        )]));
        assert!(!is_native_invocation(&[
            "untrusted@extension.invalid".into()
        ]));
    }

    /// The extension's `sendNative({type:"log",request:{events}})` payload is a wire contract:
    /// it must deserialize into the relay action and pass the browser policy unchanged.
    #[test]
    fn the_extension_log_batch_matches_the_relay_action() {
        // Captured verbatim from the real extension code (codes.js + events.js + background.js
        // driven in a Node VM, one SSDownloadEvents.log call then a flush).
        let wire = br#"{"version":2,"action":{"type":"log","request":{"events":[{"event":"ext.session","level":"info","outcome":"ok","detail":"version=unknown protocol=unknown"},{"event":"ext.capture","level":"info","outcome":"ok","code":"SSD-EXT-007","host":"www.youtube.com","detail":"hls master"}]}}}"#;
        let request: WireRequest = serde_json::from_slice(wire).expect("wire message");
        let mut action = request.action.expect("action");
        let events = match &action {
            Action::Log { request } => request.events.clone(),
            other => panic!("beklenen log eylemi, gelen: {other:?}"),
        };
        crate::validation::validate_relay_events(&events).expect("geçerli parti");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event, "ext.capture");
        assert_eq!(events[1].code.as_deref(), Some("SSD-EXT-007"));
        assert_eq!(events[1].host.as_deref(), Some("www.youtube.com"));
        validate_browser_action(&mut action).expect("eklenti olay aktarabilmeli");
    }

    #[test]
    fn browser_handoffs_validate_subtitle_addresses_cookies_and_file_names() {
        let valid = || AddRequest {
            url: "https://example.test/video".into(),
            filename: Some("clip.mkv".into()),
            external_subtitles: vec![ExternalSubtitle {
                url: "https://example.test/captions.vtt".into(),
                referer: Some("https://example.test/watch".into()),
                ..Default::default()
            }],
            session_cookies: vec![ScopedCookie {
                name: "session".into(),
                value: "value".into(),
                domain: "example.test".into(),
                path: "/".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(validate_browser_add(&mut valid()).is_ok());

        let mut local_subtitle = valid();
        local_subtitle.external_subtitles[0].url = "file:///C:/secret.txt".into();
        assert!(validate_browser_add(&mut local_subtitle).is_err());

        let mut local_referer = valid();
        local_referer.external_subtitles[0].referer = Some("ftp://example.test/".into());
        assert!(validate_browser_add(&mut local_referer).is_err());

        let mut too_many = valid();
        too_many.session_cookies =
            vec![too_many.session_cookies[0].clone(); MAX_BROWSER_COOKIES + 1];
        assert!(validate_browser_add(&mut too_many).is_err());
        let mut at_limit = valid();
        at_limit.session_cookies = vec![at_limit.session_cookies[0].clone(); MAX_BROWSER_COOKIES];
        assert!(validate_browser_add(&mut at_limit).is_ok());

        let mut huge_value = valid();
        huge_value.session_cookies[0].value = "x".repeat(8193);
        assert!(validate_browser_add(&mut huge_value).is_err());

        let mut long_name = valid();
        long_name.filename = Some("n".repeat(MAX_BROWSER_FILENAME + 1));
        assert!(validate_browser_add(&mut long_name).is_err());
    }

    /// Passive probes the extension makes in the background must not launch a desktop the
    /// user quit; the actions a user starts must still bring it up.
    #[test]
    fn only_explicit_browser_actions_start_the_desktop() {
        let log: WireRequest = serde_json::from_slice(
            br#"{"version":2,"action":{"type":"log","request":{"events":[{"event":"ext.session","level":"info","outcome":"ok"}]}}}"#,
        )
        .expect("log wire message");
        assert!(!starts_desktop(&log.action.expect("log action")));
        assert!(!starts_desktop(&Action::Capabilities));
        assert!(starts_desktop(&Action::ShowWindow));
        assert!(starts_desktop(&Action::Add {
            request: AddRequest::default()
        }));
        assert!(starts_desktop(&Action::InspectStatus {
            request_id: "inspect-1".into()
        }));
    }

    #[test]
    fn browser_status_requires_a_bounded_request_identifier() {
        let mut permitted = Action::InspectStatus {
            request_id: "inspect-1".into(),
        };
        assert!(validate_browser_action(&mut permitted).is_ok());
        let mut empty = Action::InspectStatus {
            request_id: String::new(),
        };
        assert!(validate_browser_action(&mut empty).is_err());
        let mut long = Action::InspectStatus {
            request_id: "x".repeat(129),
        };
        assert!(validate_browser_action(&mut long).is_err());
    }

    /// The connected endpoint identifies the server receiving the request, and its reply
    /// is read as a whole message rather than a partial byte-mode response.
    #[test]
    fn media_handoff_connection_resolves_the_endpoint_it_writes_to() {
        let root =
            std::env::temp_dir().join(format!("ssdownload-bridge-pipe-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root).unwrap();
        let security = pipe_security().unwrap();
        let server = create_pipe(&paths.pipe_name(), &security, true).unwrap();
        let echo = thread::spawn(move || {
            let server = server;
            let connected = unsafe { ConnectNamedPipe(server.0, ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                return false;
            }
            set_pipe_wait_mode(server.0, false);
            match wait_for_message(server.0, PIPE_IO_TIMEOUT) {
                Ok(input) => {
                    if write_message(server.0, &input).is_err() {
                        return false;
                    }
                    // Like the pipe worker, the reply is flushed before the instance goes
                    // away, so the client reads it before the connection is torn down.
                    unsafe { FlushFileBuffers(server.0) };
                    true
                }
                Err(_) => false,
            }
        });

        // The desktop listens before any client exists; this patience only absorbs test
        // scheduling and does not change what `connect_pipe` itself accepts.
        let deadline = Instant::now() + Duration::from_secs(5);
        let pipe = loop {
            match connect_pipe(&paths) {
                Ok(pipe) => break pipe,
                Err(_) if Instant::now() < deadline => thread::sleep(PIPE_POLL_INTERVAL),
                Err(error) => panic!("{error:#}"),
            }
        };
        assert_eq!(pipe_server_process_id(pipe.0).unwrap(), std::process::id());
        let input = br#"{"version":2,"action":null}"#.to_vec();
        write_message(pipe.0, &input).unwrap();
        assert_eq!(wait_for_message(pipe.0, PIPE_IO_TIMEOUT).unwrap(), input);
        assert!(echo.join().unwrap());
    }

    fn pipe_test_paths() -> AppPaths {
        let root =
            std::env::temp_dir().join(format!("ssdownload-bridge-name-{}", uuid::Uuid::new_v4()));
        AppPaths::new(root).unwrap()
    }

    /// The bridge threads of an exiting instance can hold pipe instances after its
    /// single-instance lock is free, and Windows refuses a new first instance meanwhile.
    #[test]
    fn the_first_pipe_instance_waits_until_another_holder_of_the_name_lets_go() {
        let paths = pipe_test_paths();
        let security = pipe_security().unwrap();
        let holder = create_pipe(&paths.pipe_name(), &security, true).unwrap();
        let hold = Duration::from_millis(400);
        let releasing = thread::spawn(move || {
            thread::sleep(hold);
            drop(holder);
        });

        let started = Instant::now();
        let next = create_pipe(&paths.pipe_name(), &security, true)
            .map_err(|error| format!("{error:#}"))
            .expect("the name is free once the holder closes");
        assert!(
            started.elapsed() >= hold,
            "the name was taken while the holder still had it"
        );
        releasing.join().unwrap();
        drop(next);
    }

    #[test]
    fn the_first_pipe_instance_stops_waiting_when_the_name_stays_held() {
        let paths = pipe_test_paths();
        let security = pipe_security().unwrap();
        let holder = create_pipe(&paths.pipe_name(), &security, true).unwrap();
        let wait = Duration::from_millis(300);

        let started = Instant::now();
        let refused = create_pipe_within(&paths.pipe_name(), &security, true, wait)
            .err()
            .expect("a name that stays held cannot be claimed");
        assert!(started.elapsed() >= wait, "it gave up before its window");
        assert!(
            started.elapsed() < PIPE_NAME_RELEASE_WINDOW,
            "the wait is bounded"
        );
        assert!(
            format!("{refused:#}").contains(&ERROR_ACCESS_DENIED.to_string()),
            "{refused:#}"
        );
        drop(holder);
    }

    /// What a pipe server in this process saw of one client.
    struct ServerView {
        /// The first message it received, `None` when the client sent nothing.
        message: Option<Vec<u8>>,
        /// The impersonation level the client allowed the server.
        level: Option<i32>,
        same_user: Option<bool>,
    }

    fn impersonation_level(pipe: HANDLE) -> i32 {
        use windows_sys::Win32::Security::TokenImpersonationLevel;
        assert_ne!(unsafe { ImpersonateNamedPipeClient(pipe) }, 0);
        let mut token = ptr::null_mut();
        assert_ne!(
            unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) },
            0
        );
        let token = OwnedHandle(token);
        let mut level = 0i32;
        let mut length = 0u32;
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenImpersonationLevel,
                    ptr::addr_of_mut!(level).cast(),
                    std::mem::size_of::<i32>() as u32,
                    &mut length,
                )
            },
            0
        );
        assert_ne!(unsafe { RevertToSelf() }, 0);
        level
    }

    /// Serves the pipe of a fresh profile to one client. The pipe exists when this returns, so
    /// the client needs no retry; `reply` is written back when a request arrived.
    fn serve_one_client(
        reply: Option<BridgeResponse>,
    ) -> (AppPaths, thread::JoinHandle<ServerView>, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ssdownload-bridge-pipe-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        let security = pipe_security().unwrap();
        let server = create_pipe(&paths.pipe_name(), &security, true).unwrap();
        let view = thread::spawn(move || {
            let server = server;
            let nothing = || ServerView {
                message: None,
                level: None,
                same_user: None,
            };
            if unsafe { ConnectNamedPipe(server.0, ptr::null_mut()) } == 0 {
                match unsafe { GetLastError() } {
                    ERROR_PIPE_CONNECTED => {}
                    // The client connected and closed again before this thread listened.
                    ERROR_NO_DATA => return nothing(),
                    code => panic!("ConnectNamedPipe failed with Windows error {code}"),
                }
            }
            set_pipe_wait_mode(server.0, false);
            let Ok(message) = wait_for_message(server.0, Duration::from_secs(2)) else {
                return nothing();
            };
            let level = impersonation_level(server.0);
            let same_user = same_user_client(server.0).ok();
            if let Some(reply) = reply {
                write_message(server.0, &serde_json::to_vec(&reply).unwrap()).unwrap();
                unsafe { FlushFileBuffers(server.0) };
            }
            ServerView {
                message: Some(message),
                level: Some(level),
                same_user,
            }
        });
        (paths, view, root)
    }

    #[test]
    fn a_request_reaches_a_server_of_this_user_at_the_identification_level() {
        let (paths, server, root) = serve_one_client(Some(BridgeResponse {
            result: None,
            ok: true,
            message: "tamam".into(),
            snapshot: None,
        }));
        let response = transact(&paths, Some(&Action::Status)).expect("the exchange succeeds");
        assert!(response.ok);
        assert_eq!(response.message, "tamam");

        let view = server.join().unwrap();
        let request: WireRequest =
            serde_json::from_slice(&view.message.expect("the request arrived")).unwrap();
        assert_eq!(request.version, PROTOCOL_VERSION);
        assert!(matches!(request.action, Some(Action::Status)));
        // A server another user planted under the pipe name must not be able to act as this
        // user; the owner's own check still reads the client's identity at this level.
        assert_eq!(
            view.level,
            Some(windows_sys::Win32::Security::SecurityIdentification)
        );
        assert_eq!(view.same_user, Some(true));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_pipe_served_by_another_user_gets_nothing_written() {
        let foreign: OwnerCheck = |_| Ok(false);
        let (paths, server, root) = serve_one_client(None);
        let error = connect_pipe_checked(&paths, foreign)
            .err()
            .expect("a foreign server is refused");
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::BRG_005)
        );
        assert!(server.join().unwrap().message.is_none());

        // `transact` writes only through a connection that passed the same check.
        let (paths_for_transact, server, transact_root) = serve_one_client(None);
        let error = transact_checked(&paths_for_transact, Some(&Action::Status), foreign)
            .expect_err("a foreign server gets no request");
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::BRG_005)
        );
        assert!(server.join().unwrap().message.is_none());
        drop(paths);
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(transact_root).ok();
    }

    #[test]
    fn a_server_whose_owner_cannot_be_read_is_refused() {
        let unreadable: OwnerCheck = |_| Err(anyhow!("access denied"));
        let (paths, server, root) = serve_one_client(None);
        let error = connect_pipe_checked(&paths, unreadable)
            .err()
            .expect("an unverifiable server is refused");
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::BRG_005)
        );
        assert!(
            format!("{error:#}").contains("access denied"),
            "the reason is named: {error:#}"
        );
        assert!(server.join().unwrap().message.is_none());
        std::fs::remove_dir_all(root).ok();
    }

    fn native_frame(request: &WireRequest) -> Vec<u8> {
        let body = serde_json::to_vec(request).unwrap();
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);
        frame
    }

    /// Runs the host loop over in-memory frames and returns its result with the replies.
    fn run_host(
        paths: &AppPaths,
        arguments: &[&str],
        requests: &[WireRequest],
    ) -> (Result<()>, Vec<BridgeResponse>) {
        let input: Vec<u8> = requests.iter().flat_map(native_frame).collect();
        let arguments: Vec<String> = arguments.iter().map(|item| item.to_string()).collect();
        let mut output = Vec::new();
        let result = serve_native_host(paths, &arguments, &mut &input[..], &mut output);
        let mut replies = Vec::new();
        let mut remaining = &output[..];
        while let Some(body) = read_native_message(&mut remaining).unwrap() {
            replies.push(serde_json::from_slice(&body).unwrap());
        }
        (result, replies)
    }

    fn logged_events(paths: &AppPaths) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        let Ok(entries) = std::fs::read_dir(paths.base_dir.join("logs")) else {
            return events;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                for line in std::fs::read_to_string(entry.path()).unwrap().lines() {
                    events.push(serde_json::from_str(line).unwrap());
                }
            }
        }
        events
    }

    /// A profile whose saved settings log at the default level: the host reads its logging
    /// choice from them and writes nothing when they are missing.
    fn host_profile() -> (AppPaths, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ssdownload-bridge-host-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        crate::store::Store::open(&paths.database)
            .unwrap()
            .save_settings(&Settings::default())
            .unwrap();
        (paths, root)
    }

    #[test]
    fn a_foreign_extension_origin_ends_the_host_and_is_logged() {
        let (paths, root) = host_profile();
        let (result, replies) =
            run_host(&paths, &["chrome-extension://not-the-registered-id/"], &[]);
        let error = result.expect_err("the host refuses to serve");
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::BRG_001)
        );
        assert!(replies.is_empty());
        let events = logged_events(&paths);
        let rejected = events
            .iter()
            .find(|event| event["event"] == "bridge.identity_rejected")
            .unwrap_or_else(|| panic!("no identity event in {events:?}"));
        assert_eq!(rejected["code"], "SSD-BRG-001");
        assert_eq!(rejected["channel"], "native-host");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_protocol_mismatch_is_answered_and_logged() {
        let (paths, root) = host_profile();
        let (result, replies) = run_host(
            &paths,
            &[],
            &[WireRequest {
                version: PROTOCOL_VERSION + 1,
                action: None,
            }],
        );
        result.expect("the host keeps serving");
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        assert_eq!(
            replies[0].message,
            unsupported_version(PROTOCOL_VERSION + 1)
        );
        let events = logged_events(&paths);
        let mismatch = events
            .iter()
            .find(|event| event["event"] == "bridge.protocol_mismatch")
            .unwrap_or_else(|| panic!("no mismatch event in {events:?}"));
        assert_eq!(mismatch["code"], "SSD-BRG-006");
        assert_eq!(mismatch["channel"], "native-host");
        assert!(
            mismatch["detail"]
                .as_str()
                .unwrap()
                .contains(&(PROTOCOL_VERSION + 1).to_string()),
            "{mismatch:?}"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_refused_browser_action_is_answered_and_logged_with_its_code() {
        let (paths, root) = host_profile();
        let (result, replies) = run_host(
            &paths,
            &[],
            &[WireRequest {
                version: PROTOCOL_VERSION,
                action: Some(Action::Add {
                    request: AddRequest {
                        url: "file:///C:/secret.txt".into(),
                        ..Default::default()
                    },
                }),
            }],
        );
        result.expect("the host keeps serving");
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        let events = logged_events(&paths);
        let refused = events
            .iter()
            .find(|event| event["event"] == "bridge.browser_add_rejected")
            .unwrap_or_else(|| panic!("no rejection event in {events:?}"));
        assert_eq!(refused["code"], "SSD-MED-005");
        assert_eq!(refused["outcome"], "failed");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_uneventful_host_leaves_no_log_session() {
        let (paths, root) = host_profile();
        let origin = format!("{}/", allowed_extension_origins()[0]);
        let (result, replies) = run_host(&paths, &[origin.as_str()], &[]);
        result.expect("the registered origin is served");
        assert!(replies.is_empty());
        assert!(
            logged_events(&paths).is_empty(),
            "an uneventful host writes no session banner"
        );
        std::fs::remove_dir_all(root).ok();
    }
}
