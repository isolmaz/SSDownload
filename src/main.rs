#![windows_subsystem = "windows"]

use anyhow::{bail, Context, Result};
use ssdownload::{
    app::App,
    bridge, extension, gui,
    i18n::{self, ui},
    install,
    model::*,
    paths::AppPaths,
};
use std::{
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, SetHandleInformation, ERROR_ALREADY_EXISTS,
        ERROR_FILE_NOT_FOUND, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    },
    System::{
        Console::{
            AttachConsole, GetStdHandle, SetConsoleOutputCP, ATTACH_PARENT_PROCESS,
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
        Threading::{CreateMutexW, OpenMutexW, MUTEX_MODIFY_STATE},
    },
    UI::WindowsAndMessaging::{
        FindWindowExW, GetWindowThreadProcessId, MessageBoxW, PostMessageW, MB_ICONERROR, MB_OK,
    },
};

const HELP_EN: &str = "SSDownload — native Windows file/video/audio manager\n\nssdownload.exe                         Open the desktop\n  --background                          Start in the system tray\n  --data-dir DIR                        Separate user data folder\n  --add URL                             Add to the queue\n    --kind auto|file|video|audio\n    --output DIR --name FILE\n    --connections 1..16 --sha256 HEX\n    --container mp4|mkv --max-height 1080\n    --request-id UNIQUE_ID\n    --format FORMAT_ID --audio-format best|mp3|m4a|opus|flac\n    --subtitle LANG --playlist --referer URL --start-at UNIX_OR_RFC3339\n  --request-json FILE                    Add from an AddRequest JSON file\n  --action-json FILE                     Run an action JSON file (e.g. a site crawl)\n  --inspect URL                          Print video/audio information as JSON\n  --status [--status-offset 0 --status-limit 100]   Paged status JSON\n  --diagnose [--diagnose-hours 24]      Event log summary JSON (site, code, outcome)\n  --pause ID | --resume ID | --remove ID [--delete-file]\n  --pause-all | --resume-all | --clear-completed\n  --set-settings FILE                    Save a Settings JSON file\n  --install-tools [--update-tools]       Install the verified media tools\n  --setup-tools                          Install missing media tools without starting the app\n  --browser-setup                        Open the browser setup\n  --remove-extension-policies            Remove the legacy browser extension policy\n  --register-host | --unregister-host    Native Messaging registration\n  --enable-autostart | --disable-autostart   Start at sign-in\n  --quit                                 Close the running app\n  --debug                                Developer mode (skips the wizard and the automatic tool install)\n  --version | --help\n\nDRM is not supported. Network and session limits depend on the site.\n";
const HELP_TR: &str = "SSDownload — yerel Windows dosya/video/ses yöneticisi\n\nssdownload.exe                         Masaüstünü aç\n  --background                          Sistem tepsisinde başlat\n  --data-dir DIZIN                      Ayrı kullanıcı veri klasörü\n  --add URL                             Kuyruğa ekle\n    --kind auto|file|video|audio\n    --output DIZIN --name DOSYA\n    --connections 1..16 --sha256 HEX\n    --container mp4|mkv --max-height 1080\n    --request-id BENZERSIZ_KIMLIK\n    --format FORMAT_ID --audio-format best|mp3|m4a|opus|flac\n    --subtitle DIL --playlist --referer URL --start-at UNIX_VEYA_RFC3339\n  --request-json DOSYA                   AddRequest JSON ile ekle\n  --action-json DOSYA                    Eylem JSON'u çalıştır (ör. site taraması)\n  --inspect URL                          Video/ses bilgilerini JSON yaz\n  --status [--status-offset 0 --status-limit 100]   Sayfalı durum JSON\n  --diagnose [--diagnose-hours 24]      Olay günlüğü özeti JSON (site, kod, sonuç)\n  --pause ID | --resume ID | --remove ID [--delete-file]\n  --pause-all | --resume-all | --clear-completed\n  --set-settings DOSYA                   Settings JSON kaydet\n  --install-tools [--update-tools]       Doğrulanmış medya araçlarını kur\n  --setup-tools                          Eksik medya araçlarını uygulamayı başlatmadan kur\n  --browser-setup                        Tarayıcı kurulumunu aç\n  --remove-extension-policies            Eski tarayıcı eklenti politikasını kaldır\n  --register-host | --unregister-host    Native Messaging kaydı\n  --enable-autostart | --disable-autostart   Oturum açılışında başlatma\n  --quit                                 Etkin uygulamayı kapat\n  --debug                                Geliştirici modu (sihirbazı ve otomatik araç kurulumunu atlar)\n  --version | --help\n\nDRM desteklenmez. Ağ/oturum sınırları siteye bağlıdır.\n";

/// How long a second launch keeps trying to hand its request to the instance that already
/// holds the single-instance mutex. That instance stays healthy while it loads its queue and
/// tools, inspects media, runs several jobs or sits behind a modal dialog, so a silent pipe
/// is not by itself a failure. Every attempt carries the pipe's own timeout, which is what
/// the wait really ends on, one attempt past this budget at the latest.
const INSTANCE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff between handoff attempts: they start at this wait and grow from there.
const INSTANCE_HANDOFF_MIN_SLEEP: Duration = Duration::from_millis(100);
/// Backoff cap, so a busy instance is polled at a steady pace instead of spinning.
const INSTANCE_HANDOFF_MAX_SLEEP: Duration = Duration::from_millis(500);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The CLI and the native host never open the app, so they read the profile's language here,
    // before anything is printed. Arguments that name no usable profile address the default one;
    // a profile without settings follows the Windows language.
    i18n::apply_profile_language(profile_dir(&args).ok().flatten().as_deref());
    let native = bridge::is_native_invocation(&args);
    let cli = native
        || args.iter().any(|arg| {
            arg.starts_with("--") && !matches!(arg.as_str(), "--background" | "--data-dir")
        });
    if cli && !native {
        attach_console();
    }
    // `--debug` alone still opens the desktop, so a startup failure belongs in the dialog.
    let command_line = native
        || args.iter().any(|arg| {
            arg.starts_with("--")
                && !matches!(arg.as_str(), "--background" | "--data-dir" | "--debug")
        });
    if let Err(error) = run(&args) {
        if command_line {
            if native {
                eprintln!("SSDownload native host: {error:#}");
            } else {
                let response = BridgeResponse {
                    result: None,
                    ok: false,
                    message: format!("{error:#}"),
                    snapshot: None,
                };
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = write_stdout(&format!("{json}\n"));
                }
            }
        } else {
            let text = wide(&crate::i18n::ui_format!(
                "SSDownload başlatılamadı:\n\n{error:#}",
                "SSDownload could not start:\n\n{error:#}",
                error = error
            ));
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    wide("SSDownload").as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}
fn run(args: &[String]) -> Result<()> {
    // Developer mode travels as one environment value so every module (and the
    // persisted setting written by `App::open`) reads a single source.
    if has(args, "--debug") {
        std::env::set_var("SSDOWNLOAD_DEBUG", "1");
    }
    if has(args, "--help") {
        write_stdout(ui(HELP_TR, HELP_EN))?;
        return Ok(());
    }
    if has(args, "--version") {
        write_stdout(&format!(
            "SSDownload {} / Rust + Win32 / {}\n",
            env!("CARGO_PKG_VERSION"),
            curl::Version::get().version()
        ))?;
        return Ok(());
    }
    let paths = AppPaths::discover(profile_dir(args)?.as_deref())?;
    if bridge::is_native_invocation(args) {
        return bridge::native_host(&paths);
    }
    validate_arguments(args)?;
    if has(args, "--enable-autostart") || has(args, "--disable-autostart") {
        install::set_autostart(&paths, has(args, "--enable-autostart"))?;
        return Ok(());
    }
    if has(args, "--register-host") {
        install::register_current(&paths)?;
        print_response(BridgeResponse {
            result: None,
            ok: true,
            message: ui(
                "Native Messaging kaydı tamamlandı.",
                "Native Messaging registration completed.",
            )
            .into(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--setup-tools") {
        ssdownload::tools::install_missing(&paths)?;
        return print_response(BridgeResponse {
            result: None,
            ok: true,
            message: ui(
                "Eksik bileşenler kuruldu.",
                "Missing components are installed.",
            )
            .into(),
            snapshot: None,
        });
    }
    if has(args, "--unregister-host") {
        install::unregister_current(&paths)?;
        print_response(BridgeResponse {
            result: None,
            ok: true,
            message: ui(
                "Native Messaging kaydı kaldırıldı.",
                "Native Messaging registration removed.",
            )
            .into(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--remove-extension-policies") {
        let outcome = extension::remove_legacy_policies();
        print_response(BridgeResponse {
            result: None,
            ok: outcome != extension::LegacyCleanup::NeedsElevation,
            message: outcome.summary().into(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--diagnose") {
        return diagnose_from_log(args);
    }
    if let Some(action) = parse_action(args)? {
        if matches!(action, Action::Quit) {
            let response = bridge::send(&paths, &action).context(ui(
                "Çalışan SSDownload örneği bulunamadı",
                "No running SSDownload instance was found",
            ))?;
            if response.ok {
                wait_for_instance_exit(&paths)?;
            }
            return print_response(response);
        }
        ensure_running(&paths)?;
        let wait_inspect = matches!(action, Action::Inspect { .. });
        let wait_tools = matches!(action, Action::InstallTools { .. });
        let mut response = bridge::send(&paths, &action)?;
        if !response.ok {
            return print_response(response);
        }
        let tools_id = response.snapshot.as_ref().and_then(|s| s.tools_id.clone());
        let inspect_id = response
            .snapshot
            .as_ref()
            .and_then(|s| s.inspect_id.clone());
        if wait_inspect || wait_tools {
            let deadline =
                Instant::now() + Duration::from_secs(if wait_tools { 1800 } else { 300 });
            loop {
                let busy = response.snapshot.as_ref().is_some_and(|s| {
                    if wait_inspect {
                        s.inspecting
                    } else {
                        s.installing_tools
                    }
                });
                if !busy {
                    break;
                }
                if Instant::now() > deadline {
                    bail!(
                        "{}",
                        ui(
                            "İşlem arka planda sürüyor; --status ile izleyebilirsiniz. CLI bekleme süresi doldu.",
                            "The operation is still running in the background; follow it with --status. The CLI wait time ran out."
                        )
                    );
                }
                std::thread::sleep(Duration::from_millis(300));
                let status_action = if wait_inspect {
                    Action::InspectStatus {
                        request_id: inspect_id.clone().context(ui(
                            "Kaynak istek kimliği eksik",
                            "The source request ID is missing",
                        ))?,
                    }
                } else {
                    Action::ToolsStatus {
                        request_id: tools_id.clone().context(ui(
                            "Araç işlem kimliği eksik",
                            "The tools operation ID is missing",
                        ))?,
                    }
                };
                response = bridge::send(&paths, &status_action)?;
                if !response.ok {
                    break;
                }
            }
            if let Some(snapshot) = &response.snapshot {
                if wait_tools {
                    if let Some(error) = &snapshot.tools_error {
                        response.ok = false;
                        response.message = error.clone();
                    }
                }
                if wait_inspect && snapshot.media.is_none() {
                    response.ok = false;
                    response.message = snapshot.inspect_error.clone().unwrap_or_else(|| {
                        ui("Medya çözümlenemedi.", "The media could not be analyzed.").into()
                    });
                }
            }
        }
        return print_response(response);
    }
    let mutex_name = wide(&format!(
        "Local\\{}",
        paths
            .pipe_name()
            .rsplit('\\')
            .next()
            .unwrap_or("SSDownload")
    ));
    let mut instance = claim_instance(&mutex_name)?;
    if instance.is_none() {
        // Another instance owns this profile: hand the launch to it. An accepted response
        // ends the launch at once. Waiting on the attempts alone would not be bounded - the
        // pipe client waits for the instance without a read timeout, so one attempt can
        // outlive the whole budget - therefore the handover runs on its own thread and this
        // one stops waiting on the deadline, whatever that attempt is doing. An instance that
        // is still exiting never answers; the wait ends in a normal start once its lock is free.
        let background = has(args, "--background");
        let action = if background {
            Action::Status
        } else {
            Action::ShowWindow
        };
        let (handover, outcome) = std::sync::mpsc::channel();
        let paths_for_handover = paths.clone();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new()
            .name("ssdownload-handover".into())
            .spawn(move || {
                let deadline = Instant::now() + INSTANCE_HANDOFF_TIMEOUT;
                let mut attempts = 0u32;
                loop {
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    attempts += 1;
                    let error = match bridge::send(&paths_for_handover, &action) {
                        Ok(response) if response.ok => {
                            let _ = handover.send(Handover::Accepted);
                            return;
                        }
                        Ok(response) => response.message,
                        Err(error) => format!("{error:#}"),
                    };
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        let _ = handover.send(Handover::Unanswered { attempts, error });
                        return;
                    }
                    std::thread::sleep(
                        handoff_sleep(
                            attempts - 1,
                            INSTANCE_HANDOFF_MIN_SLEEP,
                            INSTANCE_HANDOFF_MAX_SLEEP,
                        )
                        .min(remaining),
                    );
                }
            })
            .context(ui(
                "Çalışan örneğe devir iş parçacığı başlatılamadı",
                "The handover thread to the running instance could not be started",
            ))?;
        match await_handover(&outcome, &mutex_name, INSTANCE_HANDOFF_TIMEOUT)? {
            Waited::Accepted => return Ok(()),
            Waited::Vacant(guard) => {
                stop.store(true, Ordering::Relaxed);
                instance = Some(guard);
            }
            Waited::Unanswered(pipe) => {
                // Its main window proves the instance is up and merely busy, so the launch is
                // handed over instead of failed: raise it exactly as `Action::ShowWindow`
                // does. The `--background` variant asks for `Action::Status` and has no
                // window to raise.
                if !background && raise_instance_window(&paths) {
                    return Ok(());
                }
                let window = if background {
                    ui(
                        "arka plan isteği için pencere yükseltilmez",
                        "the window is not raised for a background request",
                    )
                } else {
                    ui(
                        "ana pencere de bulunamadı",
                        "the main window was not found either",
                    )
                };
                bail!(
                    "{}",
                    crate::i18n::ui_format!(
                        "SSDownload çalışıyor ancak yanıt vermiyor ({pipe}; {window}).",
                        "SSDownload is running but not responding ({pipe}; {window}).",
                        pipe = pipe,
                        window = window
                    )
                );
            }
        }
    }
    let _instance = instance;
    let app = App::open(paths)?;
    bridge::start(app.clone())?;
    let result = gui::run(app.clone(), has(args, "--background"));
    app.shutdown();
    result
}

/// The profile directory this invocation addresses: the native host uses the profile its
/// registration recorded, every other invocation `--data-dir`. `None` is the default profile.
fn profile_dir(args: &[String]) -> Result<Option<PathBuf>> {
    if bridge::is_native_invocation(args) {
        return bridge::registered_profile(&std::env::current_exe()?);
    }
    Ok(value(args, "--data-dir")?.map(PathBuf::from))
}
fn parse_action(args: &[String]) -> Result<Option<Action>> {
    if let Some(file) = value(args, "--action-json")? {
        let action: Action = serde_json::from_slice(&std::fs::read(file).context(ui(
            "İşlem JSON dosyası okunamadı",
            "The action JSON file could not be read",
        ))?)
        .context(ui("Geçersiz işlem JSON'u", "Invalid action JSON"))?;
        return Ok(Some(action));
    }
    if let Some(file) = value(args, "--request-json")? {
        let request: AddRequest = serde_json::from_slice(&std::fs::read(file).context(ui(
            "İstek JSON dosyası okunamadı",
            "The request JSON file could not be read",
        ))?)
        .context(ui("Geçersiz AddRequest JSON", "Invalid AddRequest JSON"))?;
        return Ok(Some(Action::Add { request }));
    }
    if let Some(url) = value(args, "--add")? {
        let kind = match value(args, "--kind")?.unwrap_or("auto") {
            "auto" => DownloadKind::Auto,
            "file" => DownloadKind::File,
            "video" => DownloadKind::Video,
            "audio" => DownloadKind::Audio,
            _ => bail!(
                "{}",
                ui(
                    "Tür auto/file/video/audio olmalı.",
                    "The kind must be auto/file/video/audio."
                )
            ),
        };
        let start_at = if let Some(value) = value(args, "--start-at")? {
            Some(
                value
                    .parse::<i64>()
                    .or_else(|_| {
                        chrono::DateTime::parse_from_rfc3339(value).map(|time| time.timestamp())
                    })
                    .context(ui(
                        "Başlangıç zamanı Unix saniyesi veya RFC3339 olmalı",
                        "The start time must be Unix seconds or RFC3339",
                    ))?,
            )
        } else {
            None
        };
        let request = AddRequest {
            url: url.into(),
            kind,
            filename: value(args, "--name")?.map(str::to_owned),
            directory: value(args, "--output")?.map(PathBuf::from),
            referer: value(args, "--referer")?.map(str::to_owned),
            format_id: value(args, "--format")?.map(str::to_owned),
            audio_format: value(args, "--audio-format")?.map(str::to_owned),
            container: value(args, "--container")?.map(str::to_owned),
            max_height: value(args, "--max-height")?
                .map(str::parse)
                .transpose()
                .context(ui(
                    "Çözünürlük sayı olmalı",
                    "The resolution must be a number",
                ))?,
            request_id: value(args, "--request-id")?.map(str::to_owned),
            subtitle_languages: values(args, "--subtitle")?,
            playlist: has(args, "--playlist"),
            connections: value(args, "--connections")?
                .map(str::parse)
                .transpose()
                .context(ui(
                    "Bağlantı sayısı sayı olmalı",
                    "The connection count must be a number",
                ))?,
            checksum: value(args, "--sha256")?.map(str::to_owned),
            start_at,
            ..Default::default()
        };
        return Ok(Some(Action::Add { request }));
    }
    if let Some(url) = value(args, "--inspect")? {
        return Ok(Some(Action::Inspect {
            request: InspectRequest {
                url: url.into(),
                referer: value(args, "--referer")?.map(str::to_owned),
                playlist: has(args, "--playlist"),
                ..Default::default()
            },
        }));
    }
    for flag in ["--pause", "--resume", "--remove"] {
        if let Some(id) = value(args, flag)? {
            return Ok(Some(match flag {
                "--pause" => Action::Pause { id: id.into() },
                "--resume" => Action::Resume { id: id.into() },
                _ => Action::Remove {
                    id: id.into(),
                    delete_file: has(args, "--delete-file"),
                },
            }));
        }
    }
    if let Some(file) = value(args, "--set-settings")? {
        return Ok(Some(Action::SetSettings {
            settings: serde_json::from_slice(&std::fs::read(file)?)?,
        }));
    }
    let action = if has(args, "--status") {
        Some(Action::StatusPage {
            offset: value(args, "--status-offset")?.unwrap_or("0").parse()?,
            limit: value(args, "--status-limit")?.unwrap_or("100").parse()?,
        })
    } else if has(args, "--pause-all") {
        Some(Action::PauseAll)
    } else if has(args, "--resume-all") {
        Some(Action::ResumeAll)
    } else if has(args, "--clear-completed") {
        Some(Action::ClearCompleted)
    } else if has(args, "--install-tools") || has(args, "--update-tools") {
        Some(Action::InstallTools {
            request_id: Some(uuid::Uuid::new_v4().to_string()),
            force_update: has(args, "--update-tools"),
        })
    } else if has(args, "--browser-setup") {
        Some(Action::BrowserSetup)
    } else if has(args, "--quit") {
        Some(Action::Quit)
    } else {
        None
    };
    Ok(action)
}
fn ensure_running(paths: &AppPaths) -> Result<()> {
    if let Ok(response) = bridge::send(paths, &Action::Status) {
        if response.ok {
            return Ok(());
        }
    }
    use std::os::windows::process::CommandExt;
    // Our CLI's inherited capture pipes must not remain open in the background
    // desktop process. Otherwise PowerShell/Python wait for EOF until it exits.
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let handle = GetStdHandle(stream);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    Command::new(std::env::current_exe()?)
        .arg("--background")
        .arg("--data-dir")
        .arg(&paths.base_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .context(ui(
            "SSDownload arka planda başlatılamadı",
            "SSDownload could not be started in the background",
        ))?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(response) = bridge::send(paths, &Action::Status) {
            if response.ok {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!(
        "{}",
        ui(
            "SSDownload başlatıldı ancak yerel iletişim hazır olmadı.",
            "SSDownload started but the local channel did not become ready."
        )
    )
}
fn wait_for_instance_exit(paths: &AppPaths) -> Result<()> {
    let name = wide(&format!(
        "Local\\{}",
        paths
            .pipe_name()
            .rsplit('\\')
            .next()
            .unwrap_or("SSDownload")
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let handle = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            if unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            return Err(std::io::Error::last_os_error()).context(ui(
                "Uygulamanın kapanışı izlenemedi",
                "The application's shutdown could not be monitored",
            ));
        }
        unsafe {
            CloseHandle(handle);
        }
        if Instant::now() >= deadline {
            bail!(
                "{}",
                ui(
                    "Uygulama kapanıyor; çalışan indirmelerin durması bekleniyor.",
                    "The application is closing; waiting for the running downloads to stop."
                )
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// What the handover thread has to report about a second launch.
enum Handover {
    /// The instance accepted the request; the launch is done.
    Accepted,
    /// The instance never accepted it inside the budget.
    Unanswered {
        attempts: u32,
        /// Last answer or pipe error, so the failure can name what was tried.
        error: String,
    },
}

/// How a second launch's wait for the running instance ended.
enum Waited {
    /// The instance accepted the request; the launch is done.
    Accepted,
    /// The instance exited meanwhile: this launch holds the profile's lock and starts normally.
    Vacant(InstanceGuard),
    /// The instance stayed silent for the whole budget; the text says what was tried.
    Unanswered(String),
}

/// Takes the single-instance lock of a profile; `None` means another instance holds it. The
/// lock is a named mutex, which lives until its last handle closes, so a launch that finds it
/// taken closes its own handle at once: a waiting launch must not keep the lock alive after
/// the instance that owns it has exited.
fn claim_instance(name: &[u16]) -> Result<Option<InstanceGuard>> {
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context(ui(
            "Tek örnek kilidi oluşturulamadı",
            "The single-instance lock could not be created",
        ));
    }
    let taken = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let guard = InstanceGuard(handle);
    Ok((!taken).then_some(guard))
}

/// Waits up to `budget` for the handover thread's verdict. Between its attempts the lock is
/// tried again: an instance that is still exiting has stopped answering on its pipe but not yet
/// released its lock, and once the lock is free nobody is left to hand the launch to.
fn await_handover(
    outcome: &Receiver<Handover>,
    mutex_name: &[u16],
    budget: Duration,
) -> Result<Waited> {
    let deadline = Instant::now() + budget;
    let silent = || {
        crate::i18n::ui_format!(
            "yerel iletişim kanalı {0} saniye boyunca yanıtsız kaldı",
            "the local channel did not answer for {0} seconds",
            budget.as_secs()
        )
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let verdict = match outcome.recv_timeout(remaining.min(INSTANCE_HANDOFF_MAX_SLEEP)) {
            Ok(Handover::Accepted) => return Ok(Waited::Accepted),
            Ok(Handover::Unanswered { attempts, error }) if error.is_empty() => {
                Some(crate::i18n::ui_format!(
                    "yerel iletişim kanalı {attempts} kez denendi",
                    "the local channel was tried {attempts} times",
                    attempts = attempts
                ))
            }
            Ok(Handover::Unanswered { attempts, error }) => Some(crate::i18n::ui_format!(
                "yerel iletişim kanalı {attempts} kez denendi, son yanıt: {error}",
                "the local channel was tried {attempts} times, last reply: {error}",
                attempts = attempts,
                error = error
            )),
            Err(RecvTimeoutError::Disconnected) => Some(silent()),
            Err(RecvTimeoutError::Timeout) => (Instant::now() >= deadline).then(silent),
        };
        if let Some(guard) = claim_instance(mutex_name)? {
            return Ok(Waited::Vacant(guard));
        }
        if let Some(pipe) = verdict {
            return Ok(Waited::Unanswered(pipe));
        }
    }
}

/// Wait before the next handoff attempt: it doubles from `min` and is clamped to `max`, so
/// the deadline, not a fixed attempt count, is what ends the wait. `checked_shl` keeps a
/// runaway attempt number from overflowing the multiplication.
fn handoff_sleep(attempt: u32, min: Duration, max: Duration) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    min.saturating_mul(factor).min(max)
}

/// Raises the instance that owns this profile, exactly as `Action::ShowWindow` does.
///
/// The desktop's raise path belongs to the running instance, and the pipe that carries
/// `Action::ShowWindow` is what just stayed silent, so the launcher posts the raise message
/// to that instance's main window instead: a posted message needs no answer from the busy
/// pipe, survives this process exiting, and runs the same raise on the desktop's own thread.
/// Every profile registers the same window class, so the window is the one belonging to the
/// instance the launcher was handing over to, never merely the first of that class.
/// Returns whether that window was there to receive the message.
fn raise_instance_window(paths: &AppPaths) -> bool {
    let Some(pid) = bridge::instance_process_id(paths) else {
        return false;
    };
    let class = wide(bridge::MAIN_WINDOW_CLASS);
    let mut window = std::ptr::null_mut();
    loop {
        window = unsafe {
            FindWindowExW(
                std::ptr::null_mut(),
                window,
                class.as_ptr(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return false;
        }
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == pid {
            let posted = unsafe { PostMessageW(window, bridge::WM_RAISE_WINDOW, 0, 0) };
            return posted != 0;
        }
    }
}

fn print_response(response: BridgeResponse) -> Result<()> {
    write_stdout(&format!("{}\n", serde_json::to_string(&response)?))?;
    if !response.ok {
        std::process::exit(2);
    }
    Ok(())
}
fn write_stdout(text: &str) -> Result<()> {
    let mut output = io::stdout().lock();
    match output
        .write_all(text.as_bytes())
        .and_then(|_| output.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error).context(ui(
            "Komut satırı çıktısı yazılamadı",
            "The command-line output could not be written",
        )),
    }
}
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}
fn missing_value(flag: &str) -> String {
    crate::i18n::ui_format!(
        "{flag} için değer eksik",
        "Missing value for {flag}",
        flag = flag
    )
}
fn value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        let value = args.get(index + 1).with_context(|| missing_value(flag))?;
        if value.starts_with("--") {
            bail!("{}", missing_value(flag));
        }
        Ok(Some(value))
    } else {
        Ok(None)
    }
}
fn values(args: &[String], flag: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if arg == flag {
            let value = args.get(index + 1).with_context(|| missing_value(flag))?;
            if value.starts_with("--") {
                bail!("{}", missing_value(flag));
            }
            result.push(value.clone());
        }
    }
    Ok(result)
}
fn validate_arguments(args: &[String]) -> Result<()> {
    let flags = [
        "--enable-autostart",
        "--disable-autostart",
        "--background",
        "--status",
        "--pause-all",
        "--resume-all",
        "--clear-completed",
        "--install-tools",
        "--update-tools",
        "--setup-tools",
        "--browser-setup",
        "--register-host",
        "--unregister-host",
        "--remove-extension-policies",
        "--quit",
        "--diagnose",
        "--debug",
        "--playlist",
        "--delete-file",
    ];
    let valued = [
        "--status-offset",
        "--status-limit",
        "--container",
        "--max-height",
        "--request-id",
        "--data-dir",
        "--add",
        "--request-json",
        "--action-json",
        "--kind",
        "--output",
        "--name",
        "--connections",
        "--sha256",
        "--format",
        "--audio-format",
        "--subtitle",
        "--referer",
        "--start-at",
        "--inspect",
        "--pause",
        "--resume",
        "--remove",
        "--set-settings",
        "--diagnose-hours",
    ];
    let commands = [
        "--add",
        "--request-json",
        "--action-json",
        "--inspect",
        "--status",
        "--pause",
        "--resume",
        "--remove",
        "--pause-all",
        "--resume-all",
        "--clear-completed",
        "--set-settings",
        "--install-tools",
        "--setup-tools",
        "--browser-setup",
        "--register-host",
        "--unregister-host",
        "--remove-extension-policies",
        "--quit",
        "--diagnose",
        "--enable-autostart",
        "--disable-autostart",
    ];
    let mut position = 0;
    let mut command_count = 0;
    while position < args.len() {
        let item = args[position].as_str();
        if commands.contains(&item) {
            command_count += 1;
        }
        if flags.contains(&item) {
            position += 1;
        } else if valued.contains(&item) {
            value(&args[position..], item)?;
            position += 2;
        } else {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "Bilinmeyen seçenek: {item}. --help kullanın.",
                    "Unknown option: {item}. Use --help.",
                    item = item
                )
            );
        }
    }
    // `--update-tools` accompanies `--install-tools`; on its own it is the command.
    if has(args, "--update-tools") && !has(args, "--install-tools") {
        command_count += 1;
    }
    if command_count > 1 {
        bail!(
            "{}",
            ui(
                "Bir çağrıda yalnızca bir işlem seçin.",
                "Choose only one operation per call."
            )
        );
    }
    Ok(())
}
/// Prints the event-log summary without starting the desktop application: the summary reads
/// the log files under the data directory, so it also works right after a crash.
fn diagnose_from_log(args: &[String]) -> Result<()> {
    let paths = AppPaths::discover(value(args, "--data-dir")?.map(std::path::Path::new))?;
    let hours: i64 = value(args, "--diagnose-hours")?
        .unwrap_or("24")
        .parse()
        .context(ui("Saat sayı olmalı", "Hours must be a number"))?;
    let summary = ssdownload::diagnose::summary(&paths, hours.clamp(1, 24 * 30))?;
    write_stdout(&format!("{}\n", serde_json::to_string_pretty(&summary)?))?;
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn attach_console() {
    unsafe {
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        if output.is_null() || output == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
        SetConsoleOutputCP(65001);
    }
}
struct InstanceGuard(HANDLE);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn update_tools_is_a_command_unless_it_modifies_install_tools() {
        assert!(validate_arguments(&argv(&["--install-tools", "--update-tools"])).is_ok());
        assert!(validate_arguments(&argv(&["--update-tools"])).is_ok());
        assert!(
            validate_arguments(&argv(&["--update-tools", "--quit"])).is_err(),
            "two commands must not run one and silently drop the other"
        );
        assert!(validate_arguments(&argv(&["--status", "--update-tools"])).is_err());
    }

    #[test]
    fn autostart_is_a_command_that_cannot_swallow_another() {
        assert!(validate_arguments(&argv(&["--enable-autostart"])).is_ok());
        assert!(validate_arguments(&argv(&["--disable-autostart"])).is_ok());
        assert!(validate_arguments(&argv(&["--enable-autostart", "--pause-all"])).is_err());
        assert!(validate_arguments(&argv(&["--disable-autostart", "--quit"])).is_err());
    }

    /// The handoff policy is a pure function, so the schedule a second launch walks through
    /// is checked here without waiting on anything.
    #[test]
    fn handoff_backoff_is_bounded_and_spends_the_whole_budget() {
        let (min, max, timeout) = (
            INSTANCE_HANDOFF_MIN_SLEEP,
            INSTANCE_HANDOFF_MAX_SLEEP,
            INSTANCE_HANDOFF_TIMEOUT,
        );
        assert!(min < max && max < timeout, "{min:?} {max:?} {timeout:?}");
        assert_eq!(handoff_sleep(0, min, max), min);
        assert!(handoff_sleep(1, min, max) > handoff_sleep(0, min, max));
        // No attempt waits longer than the cap or less than the attempt before it.
        let mut previous = Duration::ZERO;
        for attempt in 0..8 {
            let sleep = handoff_sleep(attempt, min, max);
            assert!(
                sleep >= previous && sleep <= max,
                "attempt {attempt}: {sleep:?}"
            );
            previous = sleep;
        }
        assert_eq!(handoff_sleep(u32::MAX, min, max), max);

        // Attempts that fit the deadline: the first is immediate, the rest pay the backoff,
        // and the deadline is spent by the time the loop gives up - by at most one wait.
        let mut attempts = 1u32;
        let mut waited = Duration::ZERO;
        while waited < timeout {
            waited += handoff_sleep(attempts - 1, min, max);
            attempts += 1;
        }
        assert!(waited >= timeout && waited <= timeout + max, "{waited:?}");
        assert!(
            attempts > 60 && attempts < 1000,
            "a {timeout:?} budget must be many tries, not a handful: {attempts}"
        );
    }

    fn scratch_lock() -> Vec<u16> {
        wide(&format!("Local\\SSDownload-test-{}", uuid::Uuid::new_v4()))
    }

    /// Owns a lock on another thread, the way a running instance does, until released.
    struct Holder {
        release: std::sync::mpsc::Sender<()>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Holder {
        fn hold(name: &[u16]) -> Self {
            let name = name.to_vec();
            let (release, released) = std::sync::mpsc::channel();
            let (ready, held) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let owner = claim_instance(&name).unwrap().expect("the lock is free");
                ready.send(()).unwrap();
                let _ = released.recv();
                drop(owner);
            });
            held.recv().unwrap();
            Self { release, thread }
        }

        fn release(self) {
            self.release.send(()).unwrap();
            self.thread.join().unwrap();
        }
    }

    #[test]
    fn a_launch_that_finds_the_lock_taken_does_not_keep_it_alive() {
        let name = scratch_lock();
        let owner = claim_instance(&name)
            .unwrap()
            .expect("a free lock is taken");
        assert!(claim_instance(&name).unwrap().is_none());
        assert!(claim_instance(&name).unwrap().is_none());
        drop(owner);
        assert!(
            claim_instance(&name).unwrap().is_some(),
            "the failed claims left no handle behind, so the lock is free once its owner is gone"
        );
    }

    #[test]
    fn a_launch_waiting_for_an_exiting_instance_starts_once_its_lock_is_free() {
        let name = scratch_lock();
        let instance = Holder::hold(&name);
        // The exiting instance has closed its pipe, so the handover thread never succeeds. Its
        // lock goes after the wait found it taken at least once.
        let (_handover, outcome) = std::sync::mpsc::channel();
        let exiting = std::thread::spawn(move || {
            std::thread::sleep(INSTANCE_HANDOFF_MAX_SLEEP + Duration::from_millis(300));
            instance.release();
        });
        let started = Instant::now();
        let waited = await_handover(&outcome, &name, Duration::from_secs(5)).unwrap();
        assert!(
            matches!(waited, Waited::Vacant(_)),
            "the launch takes the free lock instead of reporting a silent instance"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        exiting.join().unwrap();
    }

    #[test]
    fn a_free_lock_wins_over_a_late_unanswered_verdict() {
        let name = scratch_lock();
        let (handover, outcome) = std::sync::mpsc::channel();
        handover
            .send(Handover::Unanswered {
                attempts: 3,
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            await_handover(&outcome, &name, Duration::from_secs(3)).unwrap(),
            Waited::Vacant(_)
        ));
    }

    #[test]
    fn a_launch_gives_up_when_the_instance_keeps_its_lock_and_stays_silent() {
        let name = scratch_lock();
        let instance = Holder::hold(&name);
        let (_handover, outcome) = std::sync::mpsc::channel();
        let started = Instant::now();
        let waited = await_handover(&outcome, &name, Duration::from_millis(700)).unwrap();
        match waited {
            Waited::Unanswered(text) => assert!(!text.is_empty()),
            _ => panic!("a silent instance that still holds its lock is reported"),
        }
        assert!(started.elapsed() >= Duration::from_millis(700));
        assert!(started.elapsed() < Duration::from_secs(5));
        instance.release();
    }

    #[test]
    fn an_accepted_handover_and_a_vanished_handover_thread_end_the_wait_at_once() {
        let name = scratch_lock();
        let instance = Holder::hold(&name);
        let (handover, outcome) = std::sync::mpsc::channel();
        handover.send(Handover::Accepted).unwrap();
        assert!(matches!(
            await_handover(&outcome, &name, Duration::from_secs(20)).unwrap(),
            Waited::Accepted
        ));

        let (handover, outcome) = std::sync::mpsc::channel::<Handover>();
        drop(handover);
        let started = Instant::now();
        assert!(matches!(
            await_handover(&outcome, &name, Duration::from_secs(20)).unwrap(),
            Waited::Unanswered(_)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a thread that is gone cannot answer, so the wait does not run to the deadline"
        );
        instance.release();
    }
}
