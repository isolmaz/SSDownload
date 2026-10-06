#![windows_subsystem = "windows"]

use anyhow::{bail, Context, Result};
use ssdownload::{app::App, bridge, extension, gui, install, model::*, paths::AppPaths};
use std::{
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
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

const HELP:&str="SSDownload — yerel Windows dosya/video/ses yöneticisi\n\nssdownload.exe                         Masaüstünü aç\n  --background                          Sistem tepsisinde başlat\n  --data-dir DIZIN                      Ayrı kullanıcı veri klasörü\n  --add URL                             Kuyruğa ekle\n    --kind auto|file|video|audio\n    --output DIZIN --name DOSYA\n    --connections 1..16 --sha256 HEX\n    --container mp4|mkv --max-height 1080\n    --request-id BENZERSIZ_KIMLIK\n    --format FORMAT_ID --audio-format best|mp3|m4a|opus|flac\n    --subtitle DIL --playlist --referer URL --start-at UNIX_VEYA_RFC3339\n  --request-json DOSYA                   AddRequest JSON ile ekle\n  --action-json DOSYA                    Eylem JSON'u çalıştır (ör. site taraması)\n  --inspect URL                          Video/ses bilgilerini JSON yaz\n  --status [--status-offset 0 --status-limit 100]   Sayfalı durum JSON\n  --diagnose [--diagnose-hours 24]      Olay günlüğü özeti JSON (site, kod, sonuç)\n  --pause ID | --resume ID | --remove ID [--delete-file]\n  --pause-all | --resume-all | --clear-completed\n  --set-settings DOSYA                   Settings JSON kaydet\n  --install-tools [--update-tools]       Doğrulanmış medya araçlarını kur\n  --browser-setup                        Tarayıcı kurulumunu aç\n  --install-extension                    Tarayıcı eklenti politikasını yaz\n  --remove-extension-policies            Tarayıcı eklenti politikasını kaldır\n  --register-host | --unregister-host    Native Messaging kaydı\n  --enable-autostart | --disable-autostart   Oturum açılışında başlatma\n  --quit                                 Etkin uygulamayı kapat\n  --debug                                Geliştirici modu (sihirbazı ve otomatik araç kurulumunu atlar)\n  --version | --help\n\nDRM desteklenmez. Ağ/oturum sınırları siteye bağlıdır.\n";

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
    let native = bridge::is_native_invocation(&args);
    let cli = native
        || args.iter().any(|arg| {
            arg.starts_with("--") && !matches!(arg.as_str(), "--background" | "--data-dir")
        });
    if cli && !native {
        attach_console();
    }
    if let Err(error) = run(&args) {
        if cli {
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
            let text = wide(&format!("SSDownload başlatılamadı:\n\n{error:#}"));
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
        write_stdout(HELP)?;
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
    let mut data_dir = value(args, "--data-dir")?.map(PathBuf::from);
    if bridge::is_native_invocation(args) {
        data_dir = bridge::registered_profile(&std::env::current_exe()?)?;
    }
    let paths = AppPaths::discover(data_dir.as_deref())?;
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
            message: "Native Messaging kaydı tamamlandı.".into(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--unregister-host") {
        install::unregister_current(&paths)?;
        print_response(BridgeResponse {
            result: None,
            ok: true,
            message: "Native Messaging kaydı kaldırıldı.".into(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--install-extension") {
        let report = extension::install_policies()?;
        print_response(BridgeResponse {
            result: None,
            ok: true,
            message: report.summary(),
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--remove-extension-policies") {
        let summary = extension::remove_policies()?;
        print_response(BridgeResponse {
            result: None,
            ok: true,
            message: summary,
            snapshot: None,
        })?;
        return Ok(());
    }
    if has(args, "--diagnose") {
        return diagnose_from_log(args);
    }
    if let Some(action) = parse_action(args)? {
        if matches!(action, Action::Quit) {
            let response =
                bridge::send(&paths, &action).context("Çalışan SSDownload örneği bulunamadı")?;
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
                    bail!("İşlem arka planda sürüyor; --status ile izleyebilirsiniz. CLI bekleme süresi doldu.");
                }
                std::thread::sleep(Duration::from_millis(300));
                let status_action = if wait_inspect {
                    Action::InspectStatus {
                        request_id: inspect_id.clone().context("Kaynak istek kimliği eksik")?,
                    }
                } else {
                    Action::ToolsStatus {
                        request_id: tools_id.clone().context("Araç işlem kimliği eksik")?,
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
                    response.message = snapshot
                        .inspect_error
                        .clone()
                        .unwrap_or_else(|| "Medya çözümlenemedi.".into());
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
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("Tek örnek kilidi oluşturulamadı");
    }
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _guard = InstanceGuard(handle);
    if exists {
        // Another instance owns this profile: hand the launch to it. An accepted response
        // ends the launch at once. Waiting on the attempts alone would not be bounded - the
        // pipe client waits for the instance without a read timeout, so one attempt can
        // outlive the whole budget - therefore the handover runs on its own thread and this
        // one stops waiting on the deadline, whatever that attempt is doing.
        let background = has(args, "--background");
        let action = if background {
            Action::Status
        } else {
            Action::ShowWindow
        };
        let (handover, outcome) = std::sync::mpsc::channel();
        let paths_for_handover = paths.clone();
        std::thread::Builder::new()
            .name("ssdownload-handover".into())
            .spawn(move || {
                let deadline = Instant::now() + INSTANCE_HANDOFF_TIMEOUT;
                let mut attempts = 0u32;
                loop {
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
            .context("Çalışan örneğe devir iş parçacığı başlatılamadı")?;
        let pipe = match outcome.recv_timeout(INSTANCE_HANDOFF_TIMEOUT) {
            Ok(Handover::Accepted) => return Ok(()),
            Ok(Handover::Unanswered { attempts, error }) if error.is_empty() => {
                format!("yerel iletişim kanalı {attempts} kez denendi")
            }
            Ok(Handover::Unanswered { attempts, error }) => {
                format!("yerel iletişim kanalı {attempts} kez denendi, son yanıt: {error}")
            }
            Err(_) => format!(
                "yerel iletişim kanalı {} saniye boyunca yanıtsız kaldı",
                INSTANCE_HANDOFF_TIMEOUT.as_secs()
            ),
        };
        // Its main window proves the instance is up and merely busy, so the launch is handed
        // over instead of failed: raise it exactly as `Action::ShowWindow` does. The
        // `--background` variant asks for `Action::Status` and has no window to raise.
        if !background && raise_instance_window(&paths) {
            return Ok(());
        }
        bail!(
            "SSDownload çalışıyor ancak yanıt vermiyor ({pipe}; {}).",
            if background {
                "arka plan isteği için pencere yükseltilmez"
            } else {
                "ana pencere de bulunamadı"
            }
        );
    }
    let app = App::open(paths)?;
    bridge::start(app.clone())?;
    let result = gui::run(app.clone(), has(args, "--background"));
    app.shutdown();
    result
}

fn parse_action(args: &[String]) -> Result<Option<Action>> {
    if let Some(file) = value(args, "--action-json")? {
        let action: Action =
            serde_json::from_slice(&std::fs::read(file).context("İşlem JSON dosyası okunamadı")?)
                .context("Geçersiz işlem JSON'u")?;
        return Ok(Some(action));
    }
    if let Some(file) = value(args, "--request-json")? {
        let request: AddRequest =
            serde_json::from_slice(&std::fs::read(file).context("İstek JSON dosyası okunamadı")?)
                .context("Geçersiz AddRequest JSON")?;
        return Ok(Some(Action::Add { request }));
    }
    if let Some(url) = value(args, "--add")? {
        let kind = match value(args, "--kind")?.unwrap_or("auto") {
            "auto" => DownloadKind::Auto,
            "file" => DownloadKind::File,
            "video" => DownloadKind::Video,
            "audio" => DownloadKind::Audio,
            _ => bail!("Tür auto/file/video/audio olmalı."),
        };
        let start_at = if let Some(value) = value(args, "--start-at")? {
            Some(
                value
                    .parse::<i64>()
                    .or_else(|_| {
                        chrono::DateTime::parse_from_rfc3339(value).map(|time| time.timestamp())
                    })
                    .context("Başlangıç zamanı Unix saniyesi veya RFC3339 olmalı")?,
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
                .context("Çözünürlük sayı olmalı")?,
            request_id: value(args, "--request-id")?.map(str::to_owned),
            subtitle_languages: values(args, "--subtitle")?,
            playlist: has(args, "--playlist"),
            connections: value(args, "--connections")?
                .map(str::parse)
                .transpose()
                .context("Bağlantı sayısı sayı olmalı")?,
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
        .context("SSDownload arka planda başlatılamadı")?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        if let Ok(response) = bridge::send(paths, &Action::Status) {
            if response.ok {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    bail!("SSDownload başlatıldı ancak yerel iletişim hazır olmadı.")
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
            return Err(std::io::Error::last_os_error()).context("Uygulamanın kapanışı izlenemedi");
        }
        unsafe {
            CloseHandle(handle);
        }
        if Instant::now() >= deadline {
            bail!("Uygulama kapanıyor; çalışan indirmelerin durması bekleniyor.");
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
        Err(error) => Err(error).context("Komut satırı çıktısı yazılamadı"),
    }
}
fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}
fn value<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>> {
    if let Some(index) = args.iter().position(|arg| arg == flag) {
        let value = args
            .get(index + 1)
            .with_context(|| format!("{flag} için değer eksik"))?;
        if value.starts_with("--") {
            bail!("{flag} için değer eksik");
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
            let value = args
                .get(index + 1)
                .with_context(|| format!("{flag} için değer eksik"))?;
            if value.starts_with("--") {
                bail!("{flag} için değer eksik");
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
        "--browser-setup",
        "--register-host",
        "--unregister-host",
        "--install-extension",
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
        "--browser-setup",
        "--register-host",
        "--unregister-host",
        "--install-extension",
        "--remove-extension-policies",
        "--quit",
        "--diagnose",
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
            bail!("Bilinmeyen seçenek: {item}. --help kullanın.");
        }
    }
    if command_count > 1 {
        bail!("Bir çağrıda yalnızca bir işlem seçin.");
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
        .context("Saat sayı olmalı")?;
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
}
