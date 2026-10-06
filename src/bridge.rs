use crate::{
    app::App,
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
        CloseHandle, GetLastError, LocalFree, ERROR_FILE_NOT_FOUND, ERROR_NO_DATA,
        ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, FILETIME, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
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
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
            ImpersonateNamedPipeClient, PeekNamedPipe, SetNamedPipeHandleState, PIPE_NOWAIT,
            PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
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
const PIPE_WORKERS: u32 = 8;
const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(15);
/// Bounded reconnect window shared by both client transactions. It only covers failures
/// observed before a request could have been written to the connected pipe.
const PIPE_RECONNECT_WINDOW: Duration = Duration::from_millis(250);
/// Main window of the desktop instance as another process sees it: the class name it
/// registers (`gui/native/mod.rs`) and the message that makes it raise that window exactly the
/// way `Action::ShowWindow` does. The pipe is what a second launch normally uses, so both
/// values are a cross-process contract and live here, next to the foreground grant.
pub const MAIN_WINDOW_CLASS: &str = "SSDownload.MainWindow";
pub const WM_RAISE_WINDOW: u32 = WM_APP + 36;

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

fn win32_error(context: &str) -> anyhow::Error {
    let code = unsafe { GetLastError() };
    anyhow!("{context} (Windows hata kodu {code})")
}

fn token_user_sid(token: windows_sys::Win32::Foundation::HANDLE) -> Result<Vec<u8>> {
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(win32_error("Kullanıcı güvenlik kimliği okunamadı"));
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
        return Err(win32_error("Kullanıcı güvenlik kimliği okunamadı"));
    }
    Ok(data)
}

fn current_user_sid_string() -> Result<String> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(win32_error("Kullanıcı erişim belirteci açılamadı"));
    }
    let token = OwnedHandle(token);
    let data = token_user_sid(token.0)?;
    let user = unsafe { data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
    let mut sid_text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_text) } == 0 {
        return Err(win32_error("Kullanıcı güvenlik kimliği dönüştürülemedi"));
    }
    let mut length = 0;
    unsafe {
        while *sid_text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, length) });
    unsafe { LocalFree(sid_text.cast()) };
    result.context("Kullanıcı güvenlik kimliği geçersiz UTF-16 içeriyor")
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
        return Err(win32_error("Named pipe güvenlik tanımı oluşturulamadı"));
    }
    Ok(PipeSecurity(descriptor))
}

fn create_pipe(name: &str, security: &PipeSecurity, first: bool) -> Result<OwnedHandle> {
    let name = wide(name);
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
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
    if handle == INVALID_HANDLE_VALUE {
        Err(win32_error(
            "SSDownload yerel iletişim kanalı oluşturulamadı",
        ))
    } else {
        Ok(OwnedHandle(handle))
    }
}

fn same_user_client(pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<bool> {
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(win32_error("Named pipe istemcisi doğrulanamadı"));
    }
    let result = (|| {
        let mut client_token = ptr::null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut client_token) } == 0 {
            return Err(win32_error("İstemci erişim belirteci okunamadı"));
        }
        let client_token = OwnedHandle(client_token);
        let client_data = token_user_sid(client_token.0)?;
        let client_user = unsafe { client_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };

        // OpenProcessToken while impersonating still addresses this server process's primary token.
        let mut server_token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut server_token) } == 0 {
            return Err(win32_error("Sunucu erişim belirteci okunamadı"));
        }
        let server_token = OwnedHandle(server_token);
        let server_data = token_user_sid(server_token.0)?;
        let server_user = unsafe { server_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };
        Ok(unsafe {
            windows_sys::Win32::Security::EqualSid(client_user.User.Sid, server_user.User.Sid) != 0
        })
    })();
    if unsafe { RevertToSelf() } == 0 {
        return Err(win32_error("Named pipe kimliğinden çıkılamadı"));
    }
    result
}

/// Confirms `pid` runs under this user's identity before anything is handed to it. The pipe
/// DACL already limits connections to this user; the endpoint owner is checked again because
/// a foreground permission is transferred from the identity of that process, not from text.
fn same_user_process(pid: u32) -> Result<bool> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(win32_error("Named pipe sunucu süreci açılamadı"));
    }
    let process = OwnedHandle(process);
    let mut endpoint_token = ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut endpoint_token) } == 0 {
        return Err(win32_error("Sunucu erişim belirteci okunamadı"));
    }
    let endpoint_token = OwnedHandle(endpoint_token);
    let endpoint_data = token_user_sid(endpoint_token.0)?;
    let endpoint_user = unsafe { endpoint_data.as_ptr().cast::<TOKEN_USER>().read_unaligned() };

    let mut own_token = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own_token) } == 0 {
        return Err(win32_error("Kullanıcı erişim belirteci açılamadı"));
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
            return Err(win32_error("Named pipe verisi okunamadı"));
        }
        let size = available.max(message_left) as usize;
        if size > MAX_PIPE_MESSAGE {
            bail!(
                "Yerel iletişim iletisi {} bayt sınırını aşıyor",
                MAX_PIPE_MESSAGE
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
                return Err(win32_error("Named pipe iletisi okunamadı"));
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
            "Yerel iletişim yanıtı {} bayt sınırını aşıyor",
            MAX_PIPE_MESSAGE
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
        return Err(win32_error("Named pipe yanıtı yazılamadı"));
    }
    if written as usize != data.len() {
        bail!("Named pipe yanıtı eksik yazıldı");
    }
    Ok(())
}

/// Opens one message-mode connection to the desktop pipe as this user. Retrying is limited to
/// failures observed before anything is written, exactly like `transact`; a connection that
/// was established is never reopened, so a request is transmitted at most once.
fn connect_pipe(paths: &AppPaths) -> Result<OwnedHandle> {
    let pipe_name = wide(&paths.pipe_name());
    let reconnect_until = Instant::now() + PIPE_RECONNECT_WINDOW;
    loop {
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            let pipe = OwnedHandle(handle);
            // The server end is a message pipe and the client defaults to byte mode, where a
            // read can return a partial message. `CallNamedPipeW` sets this mode implicitly.
            let mode = PIPE_READMODE_MESSAGE;
            if unsafe { SetNamedPipeHandleState(pipe.0, &mode, ptr::null(), ptr::null()) } == 0 {
                return Err(win32_error("Named pipe okuma kipi ayarlanamadı"));
            }
            return Ok(pipe);
        }
        let code = unsafe { GetLastError() };
        if matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
            && Instant::now() < reconnect_until
        {
            thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }
        crate::bail_code!(
            crate::error_codes::BRG_005,
            "SSDownload çalışan örneğine bağlanılamadı (Windows hata kodu {code})"
        );
    }
}

/// The process owning the server end of a connected pipe, as reported by the kernel for that
/// endpoint. Never derived from request data.
fn pipe_server_process_id(pipe: HANDLE) -> Result<u32> {
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 || pid == 0 {
        return Err(win32_error("Named pipe sunucu süreci belirlenemedi"));
    }
    Ok(pid)
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
        bail!("Named pipe sunucusu geçerli kullanıcıya ait değil");
    }
    if unsafe { AllowSetForegroundWindow(desktop_pid) } == 0 {
        return Err(win32_error("Ön plan izni masaüstüne devredilemedi"));
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
        bail!("Named pipe istemcisi geçerli kullanıcıya ait değil");
    }
    let mut response_offset = 0;
    let response = match serde_json::from_slice::<WireRequest>(&input) {
        Ok(request) if request.version != PROTOCOL_VERSION => BridgeResponse {
            result: None,
            ok: false,
            message: format!("Desteklenmeyen iletişim sürümü: {}", request.version),
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
                        message: "İstek kabul edildi".into(),
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
                message: "SSDownload çalışıyor".into(),
                snapshot: None,
            },
        },
        Err(error) => BridgeResponse {
            result: None,
            ok: false,
            message: format!("Geçersiz yerel iletişim iletisi: {error}"),
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
                    response.message = "Tek iş izin verilen ileti boyutunu aşıyor".into();
                    response.snapshot = None;
                    continue;
                }
                response.message =
                    "Durum özeti son işleri gösteriyor; tüm geçmiş masaüstünde mevcut.".into();
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
            .context("Yerel iletişim iş parçacığı başlatılamadı")?;
    }
    Ok(())
}

fn transact(paths: &AppPaths, action: Option<&Action>) -> Result<BridgeResponse> {
    let request = WireRequest {
        version: PROTOCOL_VERSION,
        action: action.cloned(),
    };
    let input = serde_json::to_vec(&request)?;
    if input.len() > MAX_PIPE_MESSAGE {
        crate::bail_code!(crate::error_codes::BRG_002);
    }
    let pipe_name = wide(&paths.pipe_name());
    let mut output = vec![0u8; MAX_PIPE_MESSAGE];
    let mut read = 0u32;
    let reconnect_until = Instant::now() + PIPE_RECONNECT_WINDOW;
    loop {
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::CallNamedPipeW(
                pipe_name.as_ptr(),
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut read,
                PIPE_IO_TIMEOUT.as_millis() as u32,
            )
        };
        if ok != 0 {
            break;
        }
        let code = unsafe { GetLastError() };
        // The single-instance server briefly closes/recreates its pipe after each
        // client. These errors occur before a request is sent, so retry is safe.
        if matches!(code, ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
            && Instant::now() < reconnect_until
        {
            thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }
        crate::bail_code!(
            crate::error_codes::BRG_005,
            "SSDownload çalışan örneğine bağlanılamadı (Windows hata kodu {code})"
        );
    }
    output.truncate(read as usize);
    serde_json::from_slice(&output).context("SSDownload yerel iletişim yanıtı geçersiz")
}

/// Sends any local application action to the already-running per-user instance.
pub fn send(paths: &AppPaths, action: &Action) -> Result<BridgeResponse> {
    transact(paths, Some(action))
}

/// Sends a validated media handoff over a connection this process owns, because
/// `CallNamedPipeW` exposes no handle to the desktop it reached. Holding the connection lets
/// the desktop instance behind that endpoint be identified and granted foreground permission
/// before the handoff is written, so the permission is already in place when the desktop
/// handles the request and opens its picker. The handoff is written exactly once, and a
/// refused grant is only recorded: the download stays valid and just falls back to the
/// desktop's own activation.
fn send_browser_media(paths: &AppPaths, action: &Action) -> Result<BridgeResponse> {
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
        crate::logging::record(
            crate::logging::Event::warn("bridge.foreground_grant_failed")
                .outcome(crate::logging::Outcome::Failed)
                .detail(format!("{error:#}")),
        );
    }
    write_message(pipe.0, &input)?;
    // Nothing is retried from here on: the desktop may already hold the handoff.
    let output = wait_for_message(pipe.0, PIPE_IO_TIMEOUT)?;
    serde_json::from_slice(&output).context("SSDownload yerel iletişim yanıtı geçersiz")
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
    let parsed = url::Url::parse(value).context("URL geçersiz")?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        crate::bail_code!(crate::error_codes::MED_005);
    }
    if parsed.host_str().is_none() {
        bail!("URL bir sunucu adı içermiyor");
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
        bail!("Oturum üstbilgileri çok büyük");
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
        validate_browser_headers(&subtitle.headers)?;
    }
    // Page markup is report material, not a transfer input: what the extension
    // hands over stays inside this bound so a report (or any later reader) can
    // never be handed an unbounded page.
    if request
        .page_html
        .as_ref()
        .is_some_and(|html| html.len() > MAX_PAGE_HTML)
    {
        bail!("Sayfa işaretlemesi çok büyük");
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
            uuid::Uuid::parse_str(id).context("İş kimliği geçersiz")?;
            if token.is_empty() || token.len() > 128 {
                bail!("Kaynak yenileme yetkisi geçersiz");
            }
            validate_browser_add(request)?;
        }
        Action::BeginSourceRefresh { id } => {
            uuid::Uuid::parse_str(id).context("İş kimliği geçersiz")?;
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
            bail!("Tarayıcı aktarım gönderici kimliği eklentiye açık değil")
        }
        Action::BrowserTransfer {
            command: crate::browser_transfer::Command::Release { .. },
        } => bail!("Tarayıcı aktarım gönderici serbest bırakma komutu eklentiye açık değil"),
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
                bail!("Geçersiz istek kimliği");
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
            "Bu işlem tarayıcı eklentisine açık değil"
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

fn launch_origin_allowed() -> bool {
    for argument in std::env::args().skip(1) {
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
    let executable =
        std::env::current_exe().context("SSDownload çalıştırılabilir dosyası bulunamadı")?;
    Command::new(executable)
        .arg("--background")
        .arg("--data-dir")
        .arg(&paths.base_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .context("SSDownload arka planda başlatılamadı")?;
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
                            "SSDownload başlatıldı ancak yerel iletişim hazır olmadı: {first_error}"
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
                "Native Messaging uzunluk başlığı yarım kaldı"
            ),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("Native Messaging girdisi okunamadı"),
        }
    }
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_NATIVE_MESSAGE {
        crate::bail_code!(
            crate::error_codes::BRG_002,
            "Native Messaging iletisi geçersiz boyutta: {length}"
        );
    }
    let mut body = vec![0u8; length];
    input
        .read_exact(&mut body)
        .context("Native Messaging iletisi yarım kaldı")?;
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
        return Err(std::io::Error::last_os_error())
            .context("Native Messaging gönderici başlangıcı okunamadı");
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
fn record_browser_action_failure(action: &Action, error: &anyhow::Error) {
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
        Some(code) => crate::logging::record(crate::logging::Event::failure(
            name,
            code,
            format!("{error:#}"),
        )),
        None => crate::logging::record(
            crate::logging::Event::warn(name)
                .outcome(crate::logging::Outcome::Failed)
                .detail(format!("{error:#}")),
        ),
    }
}

/// Runs the browser Native Messaging stdio loop. Multiple framed messages are supported.
pub fn native_host(paths: &AppPaths) -> Result<()> {
    if !launch_origin_allowed() {
        crate::logging::record(crate::logging::Event::failure(
            "bridge.identity_rejected",
            crate::error_codes::BRG_001,
            "izin verilmeyen eklenti kaynağı",
        ));
        crate::bail_code!(crate::error_codes::BRG_001);
    }
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let body = match read_native_message(&mut input) {
            Ok(Some(body)) => body,
            Ok(None) => break,
            Err(error) => {
                let response = BridgeResponse {
                    result: None,
                    ok: false,
                    message: error.to_string(),
                    snapshot: None,
                };
                write_native_message(&mut output, &response)?;
                break;
            }
        };
        let response = match serde_json::from_slice::<WireRequest>(&body) {
            Err(error) => BridgeResponse {
                result: None,
                ok: false,
                message: format!("Geçersiz Native Messaging iletisi: {error}"),
                snapshot: None,
            },
            Ok(request) if request.version != PROTOCOL_VERSION => {
                crate::logging::record(crate::logging::Event::failure(
                    "bridge.protocol_mismatch",
                    crate::error_codes::BRG_006,
                    format!("istemci sürümü {}", request.version),
                ));
                BridgeResponse {
                    result: None,
                    ok: false,
                    message: format!("Desteklenmeyen iletişim sürümü: {}", request.version),
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
                        record_browser_action_failure(action, &error);
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
                                send_browser_media(paths, action)
                            } else {
                                transact_with_start(paths, Some(action))
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
        write_native_message(&mut output, &response)?;
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
        bail!("Native host kayıt defteri anahtarı oluşturulamadı (Windows hata kodu {status})");
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
        bail!("Native host kayıt defteri değeri yazılamadı (Windows hata kodu {status})");
    }
    Ok(())
}

fn registry_delete_tree(key_path: &str) -> Result<()> {
    let key_path = wide(key_path);
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_path.as_ptr()) };
    if status == 0 || status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
        Ok(())
    } else {
        bail!("Native host kayıt defteri anahtarı silinemedi (Windows hata kodu {status})")
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
        format!(
            "Çalıştırılabilir dosya bulunamadı: {}",
            executable.display()
        )
    })?;
    // Browser launchers do not reliably accept Rust's extended-length Windows prefix.
    let executable = executable
        .to_str()
        .context("Native host yolu geçerli Unicode değil")?;
    let executable = if let Some(path) = executable.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        executable
            .strip_prefix(r"\\?\")
            .unwrap_or(executable)
            .to_string()
    };
    let root = owned_root;
    std::fs::create_dir_all(&root).context("Native Messaging klasörü oluşturulamadı")?;
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
        .context("Chrome Native Messaging manifesti yazılamadı")?;

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
        bail!("Bu kurulum farklı bir veri profili için kayıtlı; diğer profilin kaydı korunuyor")
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
        bail!("Native host profil yolu geçersiz")
    }
    Ok(Some(profile))
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
    fn chrome_native_registration_uses_chrome_paths_only() {
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
}
