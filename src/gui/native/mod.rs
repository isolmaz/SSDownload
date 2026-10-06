use crate::{
    app::App,
    logging::Level,
    model::{
        Action, AddRequest, AppSnapshot, AudioSelection, CompletionAction, DownloadKind,
        ExternalSubtitle, FolderRule, InspectRequest, Job, JobState, MediaFormat, MediaInfo,
        MediaLaunch, QueuePolicy, Settings, SiteCrawlRequest, SiteCrawlResult, SubtitleSelection,
        SubtitleTrack, SyncPolicy, ToolProgress, UiMode, UsageModes, ONBOARDING_VERSION,
    },
};
use anyhow::{bail, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{
        Com::{CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL},
        DataExchange::{
            AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
            IsClipboardFormatAvailable, OpenClipboard, RemoveClipboardFormatListener,
            SetClipboardData,
        },
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        // GlobalFree comes from Foundation (windows-sys 0.59).
        Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        Ole::{
            OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
            DROPEFFECT_COPY, DROPEFFECT_NONE,
        },
        Threading::AttachThreadInput,
    },
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        Controls::*,
        HiDpi::{
            GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        Input::KeyboardAndMouse::{
            EnableWindow, GetFocus, RegisterHotKey, ReleaseCapture, SetCapture, SetFocus,
            TrackMouseEvent, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, TME_LEAVE,
            TME_NONCLIENT, TRACKMOUSEEVENT, VK_DELETE, VK_DOWN, VK_END, VK_F5, VK_HOME, VK_MENU,
            VK_NEXT, VK_PRIOR, VK_UP,
        },
        Shell::*,
        WindowsAndMessaging::*,
    },
};

mod alert;
mod card;
mod dialog_commands;
mod dialog_controls;
mod dialog_layout;
mod dialog_refresh;
mod theme;
use alert::*;
use card::*;
use dialog_commands::*;
use dialog_controls::*;
use dialog_layout::*;
use dialog_refresh::*;
use theme::*;

const CLASS_MAIN: &str = crate::bridge::MAIN_WINDOW_CLASS;
const CLASS_DIALOG: &str = "SSDownload.NativeDialog";
/// The themed replacement for MessageBoxW.
const CLASS_ALERT: &str = "SSDownload.AlertBox";
const APP_TITLE: &str = "SSDownload — İndirme Yöneticisi";
const TIMER_REFRESH: usize = 1;
const TIMER_DIALOG: usize = 2;
const WM_TRAY: u32 = WM_APP + 31;
const TRAY_ID: u32 = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const CF_UNICODETEXT_: u32 = 13;
const WM_DROP_TEXT: u32 = WM_APP + 32;
const WM_SHOW_WIZARD: u32 = WM_APP + 33;
/// A second launch whose handoff ran out (the pipe stayed silent) asks for the raise the
/// desktop performs for `Action::ShowWindow`; the value is shared through `bridge`.
const WM_RAISE_WINDOW: u32 = crate::bridge::WM_RAISE_WINDOW;
/// Ends the session once every modal dialog of the main window is gone.
const WM_EXIT_AFTER_DIALOGS: u32 = WM_APP + 35;
/// Polite WM_CLOSE rounds before the exit closes the rest synchronously.
const EXIT_CLOSE_ROUNDS: usize = 8;
/// Deferred menu bar repaint: the menu code draws its own highlight first.
const WM_PAINT_MENU: u32 = WM_APP + 34;
/// The shell asks a themed window to draw its own menu bar and items with these
/// undocumented messages; DefWindowProc answers them with the light theme's
/// brushes, which is what a fast cursor over the dark bar used to uncover.
const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;
const S_OK_: i32 = 0;
const E_NOINTERFACE_: i32 = 0x80004002u32 as i32;

#[repr(C)]
struct DropTarget {
    vtable: *const DropTargetVTable,
    refs: AtomicU32,
    hwnd: HWND,
}
#[repr(C)]
struct DropTargetVTable {
    query_interface: unsafe extern "system" fn(
        *mut DropTarget,
        *const windows_sys::core::GUID,
        *mut *mut c_void,
    ) -> i32,
    add_ref: unsafe extern "system" fn(*mut DropTarget) -> u32,
    release: unsafe extern "system" fn(*mut DropTarget) -> u32,
    drag_enter: unsafe extern "system" fn(*mut DropTarget, *mut c_void, u32, i64, *mut u32) -> i32,
    drag_over: unsafe extern "system" fn(*mut DropTarget, u32, i64, *mut u32) -> i32,
    drag_leave: unsafe extern "system" fn(*mut DropTarget) -> i32,
    drop: unsafe extern "system" fn(*mut DropTarget, *mut c_void, u32, i64, *mut u32) -> i32,
}
#[repr(C)]
struct DataObject {
    vtable: *const DataObjectVTable,
}
#[repr(C)]
struct DataObjectVTable {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    get_data: unsafe extern "system" fn(*mut DataObject, *const FORMATETC, *mut STGMEDIUM) -> i32,
}
const IID_IUNKNOWN: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_IDROP_TARGET: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
fn guid_equal(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
static DROP_TARGET_VTABLE: DropTargetVTable = DropTargetVTable {
    query_interface: drop_query_interface,
    add_ref: drop_add_ref,
    release: drop_release,
    drag_enter: drop_drag_enter,
    drag_over: drop_drag_over,
    drag_leave: drop_drag_leave,
    drop: drop_data,
};
unsafe extern "system" fn drop_query_interface(
    this: *mut DropTarget,
    iid: *const windows_sys::core::GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_NOINTERFACE_;
    }
    *out = null_mut();
    if !iid.is_null() && (guid_equal(&*iid, &IID_IUNKNOWN) || guid_equal(&*iid, &IID_IDROP_TARGET))
    {
        *out = this.cast();
        drop_add_ref(this);
        S_OK_
    } else {
        E_NOINTERFACE_
    }
}
unsafe extern "system" fn drop_add_ref(this: *mut DropTarget) -> u32 {
    (*this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn drop_release(this: *mut DropTarget) -> u32 {
    let left = (*this).refs.fetch_sub(1, Ordering::Release) - 1;
    if left == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(this));
    }
    left
}
unsafe extern "system" fn drop_drag_enter(
    _this: *mut DropTarget,
    _data: *mut c_void,
    _keys: u32,
    _point: i64,
    effect: *mut u32,
) -> i32 {
    if !effect.is_null() {
        *effect = DROPEFFECT_COPY;
    }
    S_OK_
}
unsafe extern "system" fn drop_drag_over(
    _this: *mut DropTarget,
    _keys: u32,
    _point: i64,
    effect: *mut u32,
) -> i32 {
    if !effect.is_null() {
        *effect = DROPEFFECT_COPY;
    }
    S_OK_
}
unsafe extern "system" fn drop_drag_leave(_this: *mut DropTarget) -> i32 {
    S_OK_
}
unsafe extern "system" fn drop_data(
    this: *mut DropTarget,
    data: *mut c_void,
    _keys: u32,
    _point: i64,
    effect: *mut u32,
) -> i32 {
    let value = ole_unicode_text(data);
    if let Some(value) = value {
        let boxed = Box::into_raw(Box::new(value));
        if PostMessageW((*this).hwnd, WM_DROP_TEXT, 0, boxed as isize) == 0 {
            drop(Box::from_raw(boxed));
            if !effect.is_null() {
                *effect = DROPEFFECT_NONE;
            }
        } else if !effect.is_null() {
            *effect = DROPEFFECT_COPY;
        }
    } else if !effect.is_null() {
        *effect = DROPEFFECT_NONE;
    }
    S_OK_
}
unsafe fn ole_unicode_text(data: *mut c_void) -> Option<String> {
    if data.is_null() {
        return None;
    }
    let object = data as *mut DataObject;
    let format = FORMATETC {
        cfFormat: CF_UNICODETEXT_ as u16,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    let mut medium: STGMEDIUM = zeroed();
    if ((*(*object).vtable).get_data)(object, &format, &mut medium) < 0 {
        return None;
    }
    let handle = medium.u.hGlobal;
    let ptr = GlobalLock(handle) as *const u16;
    if ptr.is_null() {
        ReleaseStgMedium(&mut medium);
        return None;
    }
    let capacity = (GlobalSize(handle) / 2).min(1_048_576);
    let mut len = 0usize;
    while len < capacity && *ptr.add(len) != 0 {
        len += 1;
    }
    let value = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    GlobalUnlock(handle);
    ReleaseStgMedium(&mut medium);
    Some(value)
}

// Public, stable control identifiers used by UI automation and menu routing.
pub const ID_ADD: i32 = 1001;
pub const ID_ANALYZE: i32 = 1002;
pub const ID_PAUSE_RESUME: i32 = 1003;
pub const ID_REMOVE: i32 = 1004;
pub const ID_OPEN_FOLDER: i32 = 1005;
pub const ID_BATCH: i32 = 1006;
pub const ID_IMPORT_LIST: i32 = 1007;
pub const ID_EXPORT_LIST: i32 = 1008;
pub const ID_SPEED_LIMIT: i32 = 1105;
pub const ID_SITE_LOGINS: i32 = 1209;
pub const ID_RETRY_FAILED: i32 = 1106;
pub const ID_COPY_URL: i32 = 1110;
pub const ID_REDOWNLOAD: i32 = 1111;
pub const ID_RENAME: i32 = 1112;
pub const ID_MOVE_TOP: i32 = 1113;
pub const ID_MOVE_BOTTOM: i32 = 1114;
pub const ID_START_NOW: i32 = 1115;
pub const ID_OPEN_WHEN_DONE: i32 = 1116;
pub const ID_MINI_PANEL: i32 = 1117;
pub const ID_INSTALL_UPDATE: i32 = 1217;
const ID_TRAY_ADD: i32 = 1304;
const ID_TRAY_CLIPBOARD: i32 = 1305;
const ID_TRAY_SPEED_UNLIMITED: i32 = 1307;
const ID_TRAY_SPEED_1M: i32 = 1308;
const ID_TRAY_SPEED_512K: i32 = 1309;
const ID_TRAY_SPEED_CUSTOM: i32 = 1310;
const ID_TRAY_SHUTDOWN: i32 = 1311;
const ID_TRAY_FOLDER: i32 = 1312;
const ID_TRAY_QUIET: i32 = 1313;
/// "Kuyruğa taşı" submenu: one id per queue, in settings order.
const ID_QUEUE_BASE: i32 = 1500;
const WM_SHOW_TOUR: u32 = WM_APP + 37;
const HOTKEY_ADD: i32 = 1;
const NIN_BALLOONUSERCLICK_: u32 = 0x0405;
pub const ID_SEARCH: i32 = 1010;
pub const ID_FILTER: i32 = 1011;
pub const ID_JOB_LIST: i32 = 1020;
pub const ID_DETAILS: i32 = 1021;
pub const ID_STATUS: i32 = 1022;
pub const ID_PAUSE_ALL: i32 = 1101;
pub const ID_RESUME_ALL: i32 = 1102;
pub const ID_CLEAR_COMPLETED: i32 = 1103;
pub const ID_OPEN_FILE: i32 = 1104;
pub const ID_SETTINGS: i32 = 1201;
pub const ID_TOOLS: i32 = 1202;
pub const ID_BROWSER_SETUP: i32 = 1203;
pub const ID_EXIT: i32 = 1299;
pub const ID_DIAGNOSTICS: i32 = 1204;
pub const ID_QUEUE_MANAGER: i32 = 1205;
pub const ID_FOLDER_RULES: i32 = 1206;
pub const ID_SITE_CRAWLER: i32 = 1207;
pub const ID_SYNC_MANAGER: i32 = 1208;
pub const ID_SOURCE_REFRESH: i32 = 1210;
pub const ID_EVENT_LOG: i32 = 1211;
pub const ID_JOB_LOG: i32 = 1212;
const ID_JOB_ERROR: i32 = 1213;
/// Help menu entry that starts an update check without opening Settings.
const ID_UPDATE_CHECK: i32 = 1214;
/// The user-facing retry for the automatically downloaded media components. It
/// stays in both surfaces: the automatic attempt runs without asking, so a
/// failed one needs a way back that does not depend on the developer screen.
const ID_INSTALL_TOOLS: i32 = 1216;
/// Ctrl+A over the download list. Text controls keep their own select-all.
const ID_SELECT_ALL: i32 = 1406;
const ID_SHOW_TOUR: i32 = 1420;

pub const ID_WIZARD_VIDEO: i32 = 2401;
pub const ID_WIZARD_FILE: i32 = 2402;
pub const ID_WIZARD_AUDIO: i32 = 2403;
pub const ID_WIZARD_START: i32 = 2404;
pub const ID_WIZARD_SIMPLE: i32 = 2405;
pub const ID_WIZARD_ADVANCED: i32 = 2406;
pub const ID_SETTINGS_MODE_VIDEO: i32 = 2411;
pub const ID_SETTINGS_MODE_FILE: i32 = 2412;
pub const ID_SETTINGS_MODE_AUDIO: i32 = 2413;
pub const ID_SETTINGS_REOPEN_WIZARD: i32 = 2414;
pub const ID_SETTINGS_BROWSER_TRANSFER: i32 = 2415;
pub const ID_SETTINGS_CHECK_UPDATES: i32 = 2416;
pub const ID_SETTINGS_INSTALL_EXTENSION: i32 = 2417;
pub const ID_MEDIA_AUDIO_TRACKS: i32 = 2422;
pub const ID_MEDIA_SUBTITLE_TRACKS: i32 = 2423;
pub const ID_MEDIA_EXTERNAL_URL: i32 = 2424;
pub const ID_MEDIA_EXTERNAL_LANGUAGE: i32 = 2425;
pub const ID_MEDIA_EXTERNAL_LABEL: i32 = 2426;
pub const ID_MEDIA_EXTERNAL_KIND: i32 = 2427;
pub const ID_MEDIA_EXTERNAL_DEFAULT: i32 = 2428;
pub const ID_MEDIA_EXTERNAL_ADD: i32 = 2429;
pub const ID_MEDIA_EXTERNAL_LIST: i32 = 2430;
pub const ID_MEDIA_EXTERNAL_REMOVE: i32 = 2431;
pub const ID_MEDIA_FULL_VERIFICATION: i32 = 2432;

const Q_LIST: i32 = 2501;
const Q_NAME: i32 = 2502;
const Q_CONCURRENCY: i32 = 2503;
const Q_COMPLETION: i32 = 2504;
const Q_CREATE: i32 = 2505;
const Q_RENAME: i32 = 2506;
const Q_DELETE: i32 = 2507;
const Q_UPDATE: i32 = 2508;
const Q_MOVE_JOB: i32 = 2509;
const Q_COUNTDOWN: i32 = 2510;
const Q_CANCEL_COMPLETION: i32 = 2511;
const Q_ENABLED: i32 = 2512;
const Q_WINDOWS: i32 = 2513;
const Q_QUOTA: i32 = 2514;
const Q_PROGRAM: i32 = 2515;
const Q_ARGUMENTS: i32 = 2516;
const Q_DELAY: i32 = 2517;
const R_LIST: i32 = 2520;
const R_HOST: i32 = 2521;
const R_DIR: i32 = 2522;
const R_SUBDOMAINS: i32 = 2523;
const R_FILE: i32 = 2524;
const R_VIDEO: i32 = 2525;
const R_AUDIO: i32 = 2526;
const R_PRIORITY: i32 = 2527;
const R_SAVE: i32 = 2528;
const R_REMOVE: i32 = 2529;
const R_NEW: i32 = 2530;
const C_URL: i32 = 2540;
const C_DEPTH: i32 = 2541;
const C_PAGES: i32 = 2542;
const C_CANDIDATES: i32 = 2543;
const C_SCAN: i32 = 2544;
const C_LIST: i32 = 2545;
const C_ADD: i32 = 2546;
const Y_LIST: i32 = 2560;
const Y_URL: i32 = 2561;
const Y_DIR: i32 = 2562;
const Y_INTERVAL: i32 = 2563;
const Y_OVERWRITE: i32 = 2564;
const Y_ENABLED: i32 = 2565;
const Y_SAVE: i32 = 2566;
const Y_REMOVE: i32 = 2567;
const Y_RUN: i32 = 2568;
const Y_NEW: i32 = 2569;
const E_LIST: i32 = 2601;
const E_OPEN: i32 = 2602;
const E_PACKAGE: i32 = 2603;
const E_CLOSE: i32 = 2699;
const J_LIST: i32 = 2610;
const J_COPY: i32 = 2611;
const J_OPEN: i32 = 2612;
const J_CLOSE: i32 = 2698;

const ID_TRAY_SHOW: i32 = 1301;
const ID_TRAY_PAUSE: i32 = 1302;
const ID_TRAY_RESUME: i32 = 1303;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Completed,
    Errors,
    Video,
    Audio,
    Documents,
    Archives,
    Programs,
}

/// The category filter a job belongs to, by kind first and then by file type.
fn job_category(job: &Job) -> Option<Filter> {
    match job.request.kind {
        DownloadKind::Video => return Some(Filter::Video),
        DownloadKind::Audio => return Some(Filter::Audio),
        _ => {}
    }
    let extension = Path::new(&job.name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "m4v" | "ts" => Filter::Video,
        "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "wav" | "wma" => Filter::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "rtf" | "odt"
        | "ods" | "epub" | "csv" | "md" => Filter::Documents,
        "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "iso" | "cab" => {
            Filter::Archives
        }
        "exe" | "msi" | "msix" | "appx" | "apk" | "dmg" | "deb" | "rpm" | "pkg" => Filter::Programs,
        _ => return None,
    })
}

struct MainUi {
    app: App,
    hwnd: HWND,
    font: HFONT,
    details_font: HFONT,
    icon: HICON,
    list: HWND,
    details: HWND,
    search: HWND,
    filter: HWND,
    status: HWND,
    toolbar: Vec<HWND>,
    visible_ids: Vec<String>,
    row_cache: HashMap<String, Vec<String>>,
    taskbar_created: u32,
    snapshot: AppSnapshot,
    filter_value: Filter,
    last_show_seq: u64,
    /// Last browser handoff the picker was opened for; a changed `seq` opens it.
    last_launch_seq: u64,
    seen_messages: u64,
    known_completed: HashSet<String>,
    clipboard_sequence_text: String,
    wizard_pending: bool,
    /// Surface depth and developer mode the menu bar currently reflects, so a
    /// settings change rebuilds it exactly once.
    ui_mode: UiMode,
    debug_mode: bool,
    /// Bottom-right progress of the background work the application reports on
    /// its own (the dependency install and a running update): what each has
    /// already told the user, so a repaint never repeats it.
    dependency_notice: ProgressNotice,
    update_notice: ProgressNotice,
    /// Menu bar item the keyboard or the mouse has hot, -1 when none.
    menu_hot: i32,
    /// True while a popup the bar owns is tracking: the item that opened it
    /// keeps its highlight even after the cursor moved into the popup.
    menu_open: bool,
    /// Keyboard hint mode: Windows only underlines menu mnemonics while the
    /// user is working the bar from the keyboard (Alt held, or a menu open
    /// after Alt), so the strip tracks the same state.
    menu_hint: bool,
    management_capable: bool,
    exiting: bool,
    tray_added: bool,
    drop_target: *mut DropTarget,
    dpi: u32,
    taskbar: Option<Taskbar>,
    /// Last title text set on the window, so it is rewritten only on change.
    title_text: String,
    /// Tray tooltip and progress key last shown, for the same reason.
    tray_key: String,
    /// Tray icon drawn with the progress bar, owned here.
    tray_icon: HICON,
    /// Notifications stay silent until this instant ("1 saat bildirim gösterme").
    quiet_until: Option<Instant>,
    /// The download a completion balloon refers to.
    last_completed: Option<String>,
    hotkey_registered: bool,
    /// Registration was refused (another program owns the key) this session.
    hotkey_failed: bool,
    /// Rows being dragged to a new place in the queue order.
    dragging: Option<Vec<String>>,
    /// Filter entry counts last written into the filter box.
    filter_counts: Vec<usize>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Native MB_YESNO update offer, matching the application's message style.
/// "Hayır" keeps the current version and retries on a later day; "Evet"
/// downloads, verifies and installs; the third button skips this version.
pub fn offer_update(
    manifest: &crate::update::UpdateManifest,
) -> Option<crate::update::UpdateDecision> {
    let body = format!(
        "SSDownload {} {}\n\n{}\n\n{}",
        manifest.version,
        crate::i18n::ui(
            "hazır. Şimdi güncellemek ister misiniz?",
            "is ready. Would you like to update now?",
        ),
        if manifest.notes.is_empty() {
            crate::i18n::ui("Sürüm notları verilmedi.", "No release notes provided.")
        } else {
            manifest.notes.as_str()
        },
        crate::i18n::ui(
            "Evet: indir, doğrula ve kur\nHayır: daha sonra hatırlat\nİptal: bu sürümü bir daha sorma",
            "Yes: download, verify and install\nNo: remind me later\nCancel: do not ask about this version again",
        ),
    );
    let title = format!(
        "SSDownload {} {}",
        manifest.version,
        crate::i18n::ui("güncellemesi", "update"),
    );
    let answer = unsafe {
        message(
            std::ptr::null_mut(),
            &body,
            &title,
            MB_YESNOCANCEL | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    match answer {
        IDYES => Some(crate::update::UpdateDecision::Install),
        IDCANCEL => Some(crate::update::UpdateDecision::Skip),
        _ => Some(crate::update::UpdateDecision::Later),
    }
}

/// First-run wizard auto-popup opt-out for automated desktop runs.
/// `SSDOWNLOAD_SKIP_WIZARD=1` suppresses only the automatic opening: pressing
/// Başla still records completion, and Settings can reopen the wizard any time.
fn wizard_auto_open_suppressed() -> bool {
    std::env::var_os("SSDOWNLOAD_SKIP_WIZARD").is_some_and(|value| value == "1")
}

/// Plain informational update message (already up to date, feed errors).
pub fn report_update_text(text: &str) {
    unsafe {
        message(
            std::ptr::null_mut(),
            text,
            crate::i18n::ui("SSDownload güncellemesi", "SSDownload update"),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Plain update error message.
pub fn report_update_error(text: &str) {
    unsafe {
        message(
            std::ptr::null_mut(),
            text,
            crate::i18n::ui("SSDownload güncelleme hatası", "SSDownload update error"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// Yes/No confirmation for a step that needs a UAC prompt.
fn confirm_elevated(body: &str, title: &str) -> bool {
    let answer = unsafe {
        message(
            std::ptr::null_mut(),
            body,
            title,
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    answer == IDYES
}

/// One-time startup offer to install the browser extension policies. The flag
/// is set regardless of the answer; Settings can always run the install again.
pub fn offer_extension_install(app: crate::app::App) {
    let spawned = std::thread::Builder::new()
        .name("extension-offer".into())
        .spawn(move || {
            // Give the startup update check room to speak first.
            std::thread::sleep(std::time::Duration::from_millis(2500));
            let mut next = app.snapshot().settings;
            if next.extension_offer_seen {
                return;
            }
            next.extension_offer_seen = true;
            if let Err(error) = app.update_settings_quiet(next) {
                crate::logging::record(crate::logging::Event::warn("gui.extension_offer_save").detail(format!("{error:#}")));
            }
            let installed = crate::extension::policies_installed().unwrap_or(false);
            if !crate::extension::device_is_enterprise_managed() {
                // Chrome refuses a self-hosted force-install here, and a policy
                // entry left over from an earlier version blocks the extension
                // outright, so this is the one case worth interrupting for.
                if installed
                    && confirm_elevated(
                        crate::i18n::ui(
                            "Chrome, kurumsal yönetilmeyen cihazlarda bu eklentiyi politikayla kurmayı engelliyor ve kalan politika kaydı eklentiyi bloke ediyor.\n\nKayıt yönetici onayıyla kaldırılsın mı?",
                            "Chrome blocks this extension from being force-installed by policy on devices that are not enterprise managed, and a leftover policy entry blocks the extension outright.\n\nRemove the policy entry with administrator approval?",
                        ),
                        crate::i18n::ui(
                            "SSDownload tarayıcı eklentisi",
                            "SSDownload browser extension",
                        ),
                    )
                {
                    let _ = crate::extension::run_elevated("--remove-extension-policies");
                }
                return;
            }
            if installed {
                return;
            }
            let body = crate::extension::offer_dialog_body();
            let answer = unsafe {
                message(
                    std::ptr::null_mut(),
                    &body,
                    crate::i18n::ui(
                        "SSDownload tarayıcı eklentisi",
                        "SSDownload browser extension",
                    ),
                    MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
                )
            };
            if answer != IDYES {
                return;
            }
            match crate::extension::install_policies() {
                Ok(report) => report_update_text(&report.summary()),
                Err(error) => {
                    report_update_error(&format!(
                        "{}: {error:#}",
                        crate::i18n::ui(
                            "Eklenti politikaları yazılamadı",
                            "Could not write the extension policies",
                        )
                    ))
                }
            }
        });
    if let Err(error) = spawned {
        crate::logging::record(
            crate::logging::Event::warn("gui.extension_offer_spawn").detail(format!(
                "Eklenti teklif iş parçacığı başlatılamadı: {error}"
            )),
        );
    }
}

/// Surfaces the updater result as a tray toast (success) or error dialog.
pub fn report_update_outcome(outcome: &crate::update::UpdateOutcome) {
    unsafe {
        let title = if outcome.success {
            crate::i18n::ui("SSDownload güncellemesi", "SSDownload update")
        } else {
            crate::i18n::ui("SSDownload güncelleme hatası", "SSDownload update error")
        };
        let flags = if outcome.success {
            MB_OK | MB_ICONINFORMATION
        } else {
            MB_OK | MB_ICONERROR
        };
        let mut body = outcome.message.clone();
        if let Some(folder) = &outcome.quarantine {
            body.push_str(&format!(
                "\n\n{}: {}",
                crate::i18n::ui("Karantina klasörü", "Quarantine folder"),
                folder.display()
            ));
        }
        message(
            std::ptr::null_mut(),
            &body,
            title,
            flags | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}
fn loword(v: usize) -> u16 {
    (v & 0xffff) as u16
}
fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xffff) as u16
}
fn scale(v: i32, dpi: u32) -> i32 {
    ((v as i64 * dpi as i64 + 48) / 96) as i32
}
fn mode_allows_new_job(modes: UsageModes, kind: DownloadKind) -> bool {
    modes.allows(kind)
}
fn mode_error(kind: DownloadKind) -> &'static str {
    match kind {
        DownloadKind::Auto => crate::i18n::ui(
            "Seçili kullanım amaçları otomatik indirmeye izin vermiyor.",
            "The selected usage modes do not allow automatic downloads.",
        ),
        DownloadKind::File => crate::i18n::ui(
            "Dosya indirmeleri Ayarlar'da kapalı.",
            "File downloads are disabled in Settings.",
        ),
        DownloadKind::Video => crate::i18n::ui(
            "Video indirmeleri Ayarlar'da kapalı.",
            "Video downloads are disabled in Settings.",
        ),
        DownloadKind::Audio => crate::i18n::ui(
            "Ses indirmeleri Ayarlar'da kapalı.",
            "Audio downloads are disabled in Settings.",
        ),
    }
}
unsafe fn keep_window_on_screen(hwnd: HWND) {
    let mut rect: RECT = zeroed();
    if GetWindowRect(hwnd, &mut rect) == 0 {
        return;
    }
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    if monitor.is_null() {
        return;
    }
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return;
    }
    let work = info.rcWork;
    let work_width = (work.right - work.left).max(1);
    let work_height = (work.bottom - work.top).max(1);
    let width = (rect.right - rect.left).clamp(1, work_width);
    let height = (rect.bottom - rect.top).clamp(1, work_height);
    let visible = 64;
    let left = rect.left.clamp(
        work.left - width + visible.min(width),
        work.right - visible.min(width),
    );
    let top = rect.top.clamp(
        work.top - height + visible.min(height),
        work.bottom - visible.min(height),
    );
    if left != rect.left
        || top != rect.top
        || width != rect.right - rect.left
        || height != rect.bottom - rect.top
    {
        SetWindowPos(
            hwnd,
            null_mut(),
            left,
            top,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}
fn hwnd_id(id: i32) -> HMENU {
    id as usize as HMENU
}
struct OleGuard(bool);
impl Drop for OleGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                OleUninitialize();
            }
        }
    }
}

/// Component names as the user reads them. Outside developer mode the surface
/// names what a component does instead of the upstream project it comes from;
/// logs, the job log and the diagnostics package keep the raw text, which is
/// why this is applied where the user reads it and nowhere else. Longest match
/// first, so `ffprobe` is never eaten by `FFmpeg`.
fn hidden_tool_names(text: &str) -> String {
    let mut value = std::borrow::Cow::Borrowed(text);
    for (name, replacement) in [
        ("ffprobe", crate::i18n::ui("dönüştürücü", "converter")),
        ("FFmpeg", crate::i18n::ui("dönüştürücü", "converter")),
        (
            "yt-dlp",
            crate::i18n::ui("medya bileşeni", "media component"),
        ),
        ("Deno", crate::i18n::ui("çalıştırma ortamı", "runtime")),
    ] {
        if value.contains(name) {
            value = std::borrow::Cow::Owned(value.replace(name, replacement));
        }
    }
    value.into_owned()
}
/// User-facing text of one snapshot string: raw in developer mode, named by
/// what it does otherwise.
fn display_text(debug_mode: bool, text: &str) -> String {
    if debug_mode {
        text.to_string()
    } else {
        hidden_tool_names(text)
    }
}
unsafe fn set_text(hwnd: HWND, value: &str) {
    if text(hwnd) != value {
        SetWindowTextW(hwnd, wide(value).as_ptr());
    }
}
unsafe fn text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}
/// Width of a control's current text in the control's own font.
unsafe fn text_width(hwnd: HWND) -> i32 {
    let value: Vec<u16> = text(hwnd).encode_utf16().collect();
    if value.is_empty() {
        return 0;
    }
    let dc = GetDC(hwnd);
    if dc.is_null() {
        return 0;
    }
    let font = SendMessageW(hwnd, WM_GETFONT, 0, 0);
    let previous = if font == 0 {
        null_mut()
    } else {
        SelectObject(dc, font as HGDIOBJ)
    };
    let mut size: SIZE = zeroed();
    GetTextExtentPoint32W(dc, value.as_ptr(), value.len() as i32, &mut size);
    if !previous.is_null() {
        SelectObject(dc, previous);
    }
    ReleaseDC(hwnd, dc);
    size.cx
}
unsafe fn check(hwnd: HWND) -> bool {
    SendMessageW(hwnd, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED
}
unsafe fn set_check(hwnd: HWND, value: bool) {
    SendMessageW(
        hwnd,
        BM_SETCHECK,
        if value { BST_CHECKED } else { BST_UNCHECKED } as usize,
        0,
    );
}
unsafe fn combo_index(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, CB_GETCURSEL, 0, 0) as i32
}
unsafe fn combo_select(hwnd: HWND, index: i32) {
    SendMessageW(hwnd, CB_SETCURSEL, index as usize, 0);
}
unsafe fn combo_add(hwnd: HWND, value: &str) {
    let w = wide(value);
    SendMessageW(hwnd, CB_ADDSTRING, 0, w.as_ptr() as isize);
}
unsafe fn control(parent: HWND, class: &str, label: &str, style: u32, ex: u32, id: i32) -> HWND {
    CreateWindowExW(
        ex,
        wide(class).as_ptr(),
        wide(label).as_ptr(),
        style,
        0,
        0,
        10,
        10,
        parent,
        hwnd_id(id),
        GetModuleHandleW(null()),
        null(),
    )
}
unsafe fn apply_font(hwnd: HWND, font: HFONT) {
    SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
}
unsafe fn make_font(dpi: u32, points: i32, weight: i32) -> HFONT {
    CreateFontW(
        -((points * dpi as i32) / 72),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        CLIP_DEFAULT_PRECIS as u32,
        CLEARTYPE_QUALITY as u32,
        (DEFAULT_PITCH | FF_DONTCARE) as u32,
        wide("Segoe UI").as_ptr(),
    )
}
unsafe fn dispatch(ui: &MainUi, action: Action) {
    if let Err(error) = ui.app.dispatch(action) {
        message(
            ui.hwnd,
            &format!(
                "{}:\n{error:#}",
                crate::i18n::ui(
                    "İşlem tamamlanamadı",
                    "The operation could not be completed",
                )
            ),
            "SSDownload",
            MB_OK | MB_ICONERROR,
        );
    }
}

fn state_label(job: &Job) -> String {
    let label = job.state.label();
    let phase = job.phase.trim();
    // The plain transfer phase repeats the state ("Downloading — Downloading").
    if job.state.is_active() && !phase.is_empty() && phase != label {
        format!("{label} — {phase}")
    } else {
        label.to_string()
    }
}
fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut n = value as f64;
    let mut i = 0;
    while n >= 1024.0 && i + 1 < UNITS.len() {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", value, UNITS[i])
    } else {
        format!("{n:.1} {}", UNITS[i])
    }
}
fn format_progress(job: &Job) -> String {
    match job.total {
        Some(total) if total > 0 => format!(
            "{:.1}% ({}/{})",
            job.downloaded as f64 * 100.0 / total as f64,
            format_bytes(job.downloaded),
            format_bytes(total)
        ),
        _ => format_bytes(job.downloaded),
    }
}
fn format_speed(speed: u64) -> String {
    if speed == 0 {
        "—".into()
    } else {
        format!("{}{}", format_bytes(speed), crate::i18n::ui("/sn", "/s"))
    }
}
fn format_eta(eta: Option<u64>) -> String {
    match eta {
        Some(s) => format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60),
        None => "—".into(),
    }
}
/// A job address as the duplicate check compares it: parsed and re-serialized, so
/// spelling differences the URL parser normalizes do not hide a duplicate.
fn normalized_job_url(value: &str) -> String {
    url::Url::parse(value.trim())
        .map(|url| url.to_string())
        .unwrap_or_else(|_| value.trim().to_string())
}
fn extract_urls(value: &str) -> Vec<String> {
    let mut result = Vec::new();
    for raw in value.split(|c: char| {
        c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']')
    }) {
        let candidate = raw.trim_matches(|c: char| matches!(c, ',' | ';' | '.'));
        if (candidate.starts_with("http://") || candidate.starts_with("https://"))
            && url::Url::parse(candidate).is_ok()
            && !result.iter().any(|u| u == candidate)
        {
            result.push(candidate.to_string());
        }
    }
    result
}

pub fn run(app: App, start_hidden: bool) -> Result<()> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let ole = OleGuard(OleInitialize(null()) >= 0);
        let common = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES
                | ICC_PROGRESS_CLASS
                | ICC_STANDARD_CLASSES
                | ICC_TAB_CLASSES,
        };
        if InitCommonControlsEx(&common) == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Windows ortak denetimleri başlatılamadı",
                    "Windows common controls could not be initialized",
                )
            );
        }
        register_classes()?;
        let management_capable = app
            .dispatch_result(Action::Capabilities)
            .ok()
            .and_then(|result| result.capabilities)
            .is_some_and(|value| value.protocol >= 2 && value.capability_version >= 1);
        let snapshot = app.snapshot();
        // uxtheme only honours the shell colour scheme when the process asked
        // for it before the first window exists, so the menu bar and the
        // themed controls match the stored setting from the very first frame.
        dark_app_mode(snapshot.settings.dark_mode);
        let state = Box::new(MainUi {
            app,
            hwnd: null_mut(),
            font: null_mut(),
            details_font: null_mut(),
            icon: null_mut(),
            list: null_mut(),
            details: null_mut(),
            search: null_mut(),
            filter: null_mut(),
            status: null_mut(),
            toolbar: Vec::new(),
            visible_ids: Vec::new(),
            row_cache: HashMap::new(),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            snapshot: snapshot.clone(),
            filter_value: Filter::All,
            last_show_seq: snapshot.show_window_seq,
            // The handoff sequence is process-wide monotonic, so the first tick
            // opens whatever handoff is live instead of skipping one that
            // arrived before this window existed.
            last_launch_seq: 0,
            seen_messages: snapshot.messages.last().map_or(0, |m| m.id),
            known_completed: snapshot
                .jobs
                .iter()
                .filter(|j| j.state == JobState::Completed)
                .map(|j| j.id.clone())
                .collect(),
            clipboard_sequence_text: String::new(),
            wizard_pending: snapshot.settings.onboarding_version < ONBOARDING_VERSION
                && !snapshot.settings.debug_mode,
            ui_mode: UiMode::parse(&snapshot.settings.ui_mode),
            debug_mode: snapshot.settings.debug_mode,
            dependency_notice: ProgressNotice::default(),
            update_notice: ProgressNotice::default(),
            menu_hot: -1,
            menu_open: false,
            menu_hint: false,
            management_capable,
            exiting: false,
            tray_added: false,
            drop_target: null_mut(),
            dpi: 96,
            taskbar: None,
            title_text: String::new(),
            tray_key: String::new(),
            tray_icon: null_mut(),
            quiet_until: None,
            last_completed: None,
            hotkey_registered: false,
            hotkey_failed: false,
            dragging: None,
            filter_counts: Vec::new(),
        });
        let raw = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            0,
            wide(CLASS_MAIN).as_ptr(),
            wide(crate::i18n::ui(APP_TITLE, "SSDownload — Download Manager")).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(1180, GetDpiForSystem().max(96)),
            scale(760, GetDpiForSystem().max(96)),
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            raw.cast(),
        );
        if hwnd.is_null() {
            drop(Box::from_raw(raw));
            bail!(
                "{} (Win32 {})",
                crate::i18n::ui(
                    "Ana pencere oluşturulamadı",
                    "The main window could not be created",
                ),
                GetLastError()
            );
        }
        let maximized = restore_placement(hwnd, &(*raw).snapshot.settings);
        if !start_hidden {
            ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else { SW_SHOW });
            SetForegroundWindow(hwnd);
        } else {
            ShowWindow(hwnd, SW_HIDE);
        }
        if !start_hidden
            && !(*raw).wizard_pending
            && !(*raw).snapshot.settings.tour_seen
            && !(*raw).snapshot.settings.debug_mode
        {
            PostMessageW(hwnd, WM_SHOW_TOUR, 0, 0);
        }
        UpdateWindow(hwnd);
        if !start_hidden && (*raw).wizard_pending && !wizard_auto_open_suppressed() {
            PostMessageW(hwnd, WM_SHOW_WIZARD, 0, 0);
        }
        // Non-blocking scheduled update check (daily-gated, background thread).
        crate::update::startup_check((*raw).app.clone());
        // One-time extension install offer (Settings can repeat it anytime).
        // A developer run is never interrupted by one-time prompts.
        if !(*raw).app.snapshot().settings.debug_mode
            && !(*raw).app.snapshot().settings.extension_offer_seen
        {
            crate::gui::offer_extension_install((*raw).app.clone());
        }
        // Missing media dependencies are fetched by the application itself: the
        // user never has to open a menu for them. The short delay lets the
        // window paint first, and the thread owns nothing of the UI - the
        // install reports itself through the snapshot.
        if !(*raw).app.snapshot().settings.debug_mode {
            let app = (*raw).app.clone();
            let spawned = std::thread::Builder::new()
                .name("tools-autostart".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(800));
                    let snapshot = app.snapshot();
                    if snapshot.settings.debug_mode || snapshot.installing_tools {
                        return;
                    }
                    let installed = snapshot.tools.iter().all(|tool| tool.installed);
                    // An installed toolchain older than a week is refreshed in the
                    // background, so site support keeps up with the extractor. The
                    // engine refuses the swap while media jobs run; the next start
                    // tries again.
                    let stale = installed
                        && crate::tools::toolchain_age_days(app.paths())
                            .is_some_and(|days| days >= TOOLCHAIN_REFRESH_DAYS);
                    if installed && !stale {
                        return;
                    }
                    let _ = app.dispatch(Action::InstallTools {
                        request_id: None,
                        force_update: stale,
                    });
                });
            if let Err(error) = spawned {
                crate::logging::record(
                    crate::logging::Event::warn("gui.tools_autostart_spawn").detail(format!(
                        "Araç otomatik kurulum iş parçacığı başlatılamadı: {error}"
                    )),
                );
            }
        }
        let accelerators = [
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'N' as u16,
                cmd: ID_ADD as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL | FSHIFT,
                key: b'N' as u16,
                cmd: ID_ANALYZE as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'V' as u16,
                cmd: 1403,
            },
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'F' as u16,
                cmd: 1404,
            },
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'A' as u16,
                cmd: ID_SELECT_ALL as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY,
                key: VK_F5,
                cmd: 1405,
            },
            ACCEL {
                fVirt: FVIRTKEY | FALT,
                key: VK_HOME,
                cmd: ID_MOVE_TOP as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY | FALT,
                key: VK_END,
                cmd: ID_MOVE_BOTTOM as u16,
            },
        ];
        let accel = CreateAcceleratorTableW(accelerators.as_ptr(), accelerators.len() as i32);
        let mut msg: MSG = zeroed();
        loop {
            let got = GetMessageW(&mut msg, null_mut(), 0, 0);
            if got == 0 {
                break;
            }
            if got == -1 {
                if !accel.is_null() {
                    DestroyAcceleratorTable(accel);
                }
                let error = GetLastError();
                // The window is still alive on this path: its teardown runs
                // from the destroy and the box may only be dropped once no
                // frame can touch it.
                if IsWindow(hwnd) != 0 {
                    DestroyWindow(hwnd);
                }
                drop(Box::from_raw(raw));
                drop(ole);
                bail!(
                    "{} (Win32 {error})",
                    crate::i18n::ui(
                        "Windows ileti döngüsü başarısız oldu",
                        "The Windows message loop failed",
                    )
                );
            }
            if (accel.is_null() || TranslateAcceleratorW(hwnd, accel, &msg) == 0)
                && !dialog_message(hwnd, &msg)
            {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !accel.is_null() {
            DestroyAcceleratorTable(accel);
        }
        // The main window's box is owned here, not by the window: a quit
        // posted from inside a modal dialog ends the pump while the frame is
        // still alive, and the teardown that destroys it runs from the frame.
        // The destroy comes first, so no handler and no nested frame can see a
        // freed box, and this is the only place that drops it.
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        drop(Box::from_raw(raw));
        drop(ole);
        Ok(())
    }
}

unsafe fn register_classes() -> Result<()> {
    let instance = GetModuleHandleW(null());
    let icon = {
        let own = LoadIconW(instance, 101usize as *const u16);
        if own.is_null() {
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            own
        }
    };
    let main_class_name = wide(CLASS_MAIN);
    let main_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(main_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: main_class_name.as_ptr(),
        hIconSm: icon,
    };
    if RegisterClassExW(&main_class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        bail!(
            "{} (Win32 {})",
            crate::i18n::ui(
                "Ana pencere sınıfı kaydedilemedi",
                "The main window class could not be registered",
            ),
            GetLastError()
        );
    }
    let dialog_class_name = wide(CLASS_DIALOG);
    let dialog_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(dialog_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: dialog_class_name.as_ptr(),
        hIconSm: icon,
    };
    if RegisterClassExW(&dialog_class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        bail!(
            "{} (Win32 {})",
            crate::i18n::ui(
                "İletişim penceresi sınıfı kaydedilemedi",
                "The dialog window class could not be registered",
            ),
            GetLastError()
        );
    }
    let alert_class_name = wide(CLASS_ALERT);
    let alert_class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(alert_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: icon,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
        lpszMenuName: null(),
        lpszClassName: alert_class_name.as_ptr(),
        hIconSm: icon,
    };
    if RegisterClassExW(&alert_class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        bail!(
            "{} (Win32 {})",
            crate::i18n::ui(
                "Uyarı penceresi sınıfı kaydedilemedi",
                "The alert window class could not be registered",
            ),
            GetLastError()
        );
    }
    if !register_card_class(instance, icon) {
        bail!(
            "{} (Win32 {})",
            crate::i18n::ui(
                "Kart penceresi sınıfı kaydedilemedi",
                "The card window class could not be registered",
            ),
            GetLastError()
        );
    }
    Ok(())
}

unsafe extern "system" fn main_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        let raw = create.lpCreateParams as *mut MainUi;
        (*raw).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    }
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MainUi;
    if raw.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ui = &mut *raw;
    if let Some(value) = paint_theme(hwnd, msg, wparam, lparam) {
        return value;
    }
    if msg == ui.taskbar_created && msg != 0 {
        ui.taskbar = None;
        ui.tray_added = false;
        add_tray(ui);
        return 0;
    }
    match msg {
        WM_NCPAINT => {
            // The default paints the frame and the light menu bar strip; the
            // strip is repainted here so the whole bar follows the scheme.
            let value = DefWindowProcW(hwnd, msg, wparam, lparam);
            if scheme_dark(hwnd) {
                paint_menu_strip(ui);
            }
            value
        }
        WM_NCACTIVATE => {
            // Activation and deactivation repaint the frame in the light theme,
            // menu bar included, and they do it outside WM_NCPAINT. The strip is
            // painted again on top so a focus change cannot leave the bar light.
            let value = DefWindowProcW(hwnd, msg, wparam, lparam);
            if scheme_dark(hwnd) {
                paint_menu_strip(ui);
                PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            }
            value
        }
        WM_MENUSELECT => {
            // The bar itself reports its item index here; a popup's own items
            // report their own menu. Repainting is deferred one message so the
            // menu code's own highlight lands first and is painted over.
            let menu = lparam as HMENU;
            let closing = wparam == usize::MAX;
            if !closing && !menu.is_null() && menu == GetMenu(ui.hwnd) {
                ui.menu_hot = loword(wparam) as i32;
            } else if closing || !ui.menu_open {
                // Once the bar item opened its popup, the highlight stays on
                // that item until the popup stops tracking, exactly like the
                // shell keeps it.
                ui.menu_hot = -1;
            }
            if scheme_dark(hwnd) {
                PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            }
            0
        }
        WM_SYSKEYDOWN | WM_SYSKEYUP if wparam & 0xffff == VK_MENU as usize => {
            // Windows reserves the underline for keyboard mode: it appears
            // when Alt goes down and is gone once Alt is released.
            let hint = msg == WM_SYSKEYDOWN;
            if ui.menu_hint != hint {
                ui.menu_hint = hint;
                DrawMenuBar(hwnd);
                // The menu code paints the activated bar itself, and it paints
                // after this message. The strip is repainted one message later,
                // from inside the menu loop, so the dark surface wins.
                PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ENTERMENULOOP => {
            ui.menu_open = true;
            PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_KILLFOCUS if ui.menu_hint => {
            ui.menu_hint = false;
            DrawMenuBar(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITMENULOOP => {
            ui.menu_open = false;
            ui.menu_hot = -1;
            if scheme_dark(hwnd) {
                PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            }
            0
        }
        WM_UAHDRAWMENU if scheme_dark(hwnd) => {
            // The theme asks for the bar's own drawing and hands over the
            // context it presents. That context is the only one the shell
            // blits, so the strip is drawn into it rather than into a window
            // context of our own, whose paint the shell's would cover.
            let ask = &*(lparam as *const UahMenu);
            paint_menu_strip_into(ui, ask.dc);
            1
        }
        WM_UAHDRAWMENUITEM if scheme_dark(hwnd) => {
            // The same request for one item, with a DRAWITEMSTRUCT at the head
            // of the request; its hDC is the context to draw in.
            let ask = &*(lparam as *const DRAWITEMSTRUCT);
            paint_menu_strip_into(ui, ask.hDC);
            1
        }
        WM_NCMOUSEMOVE => {
            // The shell highlights the bar's hot item in the light theme even
            // while the window is dark; it paints that box while this message
            // is handled, so the strip keeps the hot item itself and lands on
            // top of it in the same message instead of flashing light.
            let value = DefWindowProcW(hwnd, msg, wparam, lparam);
            if scheme_dark(hwnd) {
                track_nc_leave(hwnd);
                let over_bar = loword(wparam) as u32 == HTMENU;
                let menu = GetMenu(hwnd);
                let hot = if over_bar && !menu.is_null() {
                    menu_item_at(
                        hwnd,
                        menu,
                        POINT {
                            x: loword(lparam as usize) as i16 as i32,
                            y: hiword(lparam as usize) as i16 as i32,
                        },
                    )
                } else {
                    -1
                };
                // Over the bar the shell has just painted its own light box, so
                // the strip lands on top of it even when the item did not
                // change; elsewhere only a cleared highlight needs a repaint.
                if over_bar || hot != ui.menu_hot {
                    ui.menu_hot = hot;
                    paint_menu_strip(ui);
                }
            }
            value
        }
        WM_NCMOUSELEAVE => {
            // The shell repaints the item it highlighted with the light bar's
            // own colours once the cursor is gone, so the strip is repainted
            // one message later, after that paint has landed. While a popup
            // tracks, the cursor only left for the popup and the item keeps the
            // highlight the open menu gives it.
            let value = DefWindowProcW(hwnd, msg, wparam, lparam);
            if scheme_dark(hwnd) {
                if !ui.menu_open {
                    ui.menu_hot = -1;
                }
                PostMessageW(hwnd, WM_PAINT_MENU, 0, 0);
            }
            value
        }
        WM_PAINT_MENU => {
            if scheme_dark(hwnd) {
                RedrawWindow(
                    hwnd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    RDW_FRAME | RDW_INVALIDATE | RDW_UPDATENOW,
                );
            }
            0
        }
        WM_CREATE => {
            create_main_controls(ui);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            // This arrives before WM_CREATE, so the DPI has to come from the
            // window, not from state that is filled in later.
            let dpi = GetDpiForWindow(hwnd).max(96);
            let (x, y) = clamp_min_track(hwnd, dpi, 760, 560);
            info.ptMinTrackSize.x = x;
            info.ptMinTrackSize.y = y;
            0
        }
        WM_SIZE => {
            layout_main(ui);
            0
        }
        WM_DPICHANGED => {
            let suggested = &*(lparam as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            update_main_dpi(ui);
            0
        }
        WM_SETTINGCHANGE => {
            apply_dark_mode(hwnd, ui.snapshot.settings.dark_mode);
            0
        }
        WM_DISPLAYCHANGE => {
            keep_window_on_screen(hwnd);
            update_main_dpi(ui);
            0
        }
        WM_TIMER if wparam == TIMER_REFRESH => {
            refresh(ui, false);
            0
        }
        WM_COMMAND => {
            main_command(ui, loword(wparam) as i32, hiword(wparam));
            0
        }
        WM_SHOW_WIZARD => {
            if ui.wizard_pending && IsWindowVisible(hwnd) != 0 {
                let popup = GetLastActivePopup(hwnd);
                if popup.is_null() || popup == hwnd || IsWindowVisible(popup) == 0 {
                    show_usage_wizard(hwnd, ui.app.clone(), ui.snapshot.settings.clone());
                    let snapshot = ui.app.snapshot();
                    ui.wizard_pending = snapshot.settings.onboarding_version < ONBOARDING_VERSION;
                    ui.snapshot = snapshot;
                    if !ui.wizard_pending {
                        PostMessageW(hwnd, WM_SHOW_TOUR, 0, 0);
                    }
                }
            }
            0
        }
        WM_RAISE_WINDOW => {
            show_main(ui);
            0
        }
        WM_HOTKEY if wparam as i32 == HOTKEY_ADD => {
            let urls = clipboard_urls();
            SetForegroundWindow(hwnd);
            show_add_dialog(hwnd, ui.app.clone(), urls.join("\r\n"), false);
            0
        }
        WM_SHOW_TOUR => {
            show_tour(ui, false);
            0
        }
        WM_MOUSEMOVE if ui.dragging.is_some() => {
            SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
            0
        }
        WM_LBUTTONUP if ui.dragging.is_some() => {
            let ids = ui.dragging.take().unwrap_or_default();
            ReleaseCapture();
            let mut point: POINT = zeroed();
            GetCursorPos(&mut point);
            ScreenToClient(ui.list, &mut point);
            let mut hit: LVHITTESTINFO = zeroed();
            hit.pt = point;
            let index = SendMessageW(ui.list, LVM_HITTEST, 0, &mut hit as *mut _ as LPARAM);
            if index >= 0 {
                if let Some(target) = ui.visible_ids.get(index as usize).cloned() {
                    if !ids.contains(&target) {
                        dispatch(ui, Action::MoveBefore { ids, target });
                    }
                }
            }
            0
        }
        WM_CAPTURECHANGED => {
            ui.dragging = None;
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXIT_AFTER_DIALOGS => {
            // One dialog per round, each through its own WM_CLOSE, and the
            // window only goes once they are gone - or once the rounds run out
            // and the rest are forced. By then every opener frame has returned,
            // so nothing writes into the window after it is freed.
            let round = wparam;
            if round > EXIT_CLOSE_ROUNDS {
                DestroyWindow(ui.hwnd);
            } else if close_top_dialog(ui.hwnd) {
                if round == EXIT_CLOSE_ROUNDS {
                    force_owned_dialogs(ui.hwnd);
                }
                PostMessageW(ui.hwnd, WM_EXIT_AFTER_DIALOGS, round + 1, 0);
            } else {
                PostMessageW(ui.hwnd, WM_EXIT_AFTER_DIALOGS, EXIT_CLOSE_ROUNDS + 1, 0);
            }
            0
        }
        WM_NOTIFY => {
            let hdr = &*(lparam as *const NMHDR);
            if hdr.hwndFrom == ui.list && hdr.code == NM_CUSTOMDRAW {
                if scheme_dark(ui.hwnd) {
                    return list_custom_draw(ui, lparam);
                }
                return light_list_custom_draw(ui, lparam);
            }
            if hdr.hwndFrom == ui.list && hdr.code == LVN_BEGINDRAG {
                let ids = selected_ids(ui);
                if !ids.is_empty() {
                    ui.dragging = Some(ids);
                    SetCapture(hwnd);
                    SetCursor(LoadCursorW(null_mut(), IDC_SIZENS));
                }
                return 0;
            }
            if hdr.hwndFrom == ui.list {
                if hdr.code == LVN_ITEMCHANGED
                    || hdr.code == NM_CLICK
                    || hdr.code == LVN_ITEMACTIVATE
                    || hdr.code == NM_DBLCLK
                {
                    update_details(ui);
                    if hdr.code == LVN_ITEMACTIVATE {
                        if ui.snapshot.settings.double_click == "folder" {
                            open_selected_folder(ui);
                        } else {
                            open_selected_file(ui);
                        }
                    }
                } else if hdr.code == LVN_KEYDOWN {
                    let key = &*(lparam as *const NMLVKEYDOWN);
                    match key.wVKey {
                        VK_DELETE => remove_selected(ui),

                        0x20 => pause_resume_selected(ui),
                        _ => {}
                    }
                }
            }
            0
        }
        WM_CTLCOLORSTATIC => {
            // Without this the shell paints labels in the button face colour,
            // which shows up as a grey box on the window surface.
            SetTextColor(wparam as HDC, GetSysColor(COLOR_WINDOWTEXT));
            SetBkMode(wparam as HDC, TRANSPARENT as i32);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_KEYDOWN => {
            if wparam as u16 == VK_DELETE {
                remove_selected(ui);
                return 0;
            }
            if wparam as u16 == VK_F5 {
                refresh(ui, true);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DROPFILES => {
            handle_drop(ui, wparam as HDROP);
            0
        }
        WM_DROP_TEXT => {
            let value = Box::from_raw(lparam as *mut String);
            let urls = extract_urls(&value);
            if !urls.is_empty() {
                show_add_dialog(ui.hwnd, ui.app.clone(), urls.join("\r\n"), false);
            } else {
                message(
                    ui.hwnd,
                    crate::i18n::ui(
                        "Bırakılan metinde HTTP/HTTPS bağlantısı bulunamadı.",
                        "No HTTP/HTTPS link was found in the dropped text.",
                    ),
                    crate::i18n::ui("Sürükle ve bırak", "Drag and drop"),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            0
        }
        WM_CLIPBOARDUPDATE => {
            handle_clipboard_change(ui);
            0
        }
        WM_TRAY => {
            // NOTIFYICON_VERSION_4 packs the icon ID in HIWORD(lParam).
            handle_tray(ui, loword(lparam as usize) as u32);
            0
        }
        WM_CONTEXTMENU if wparam as HWND == ui.list => {
            show_job_menu(ui, lparam);
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            // The session is going down, so nothing waits for polite rounds: the
            // dialogs close through their own path right here, and the window
            // follows from the next message, which keeps every opener frame in
            // front of the destroy. This message does not block - Windows may
            // end the process as soon as it returns.
            begin_exit(ui);
            force_owned_dialogs(hwnd);
            PostMessageW(hwnd, WM_EXIT_AFTER_DIALOGS, EXIT_CLOSE_ROUNDS + 1, 0);
            0
        }
        WM_CLOSE => {
            if !ui.exiting && ui.snapshot.settings.close_to_tray {
                ShowWindow(hwnd, SW_HIDE);
                notify(
                    ui,
                    "SSDownload",
                    crate::i18n::ui(
                        "İndirmeler arka planda sürüyor. Açmak için bildirim alanı simgesine tıklayın.",
                        "Downloads keep running in the background. Click the notification area icon to open the window.",
                    ),
                    false,
                );
            } else {
                exit_app(ui);
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            KillTimer(hwnd, TIMER_REFRESH);
            RemoveClipboardFormatListener(hwnd);
            DragAcceptFiles(hwnd, 0);
            if !ui.drop_target.is_null() {
                RevokeDragDrop(hwnd);
                drop_release(ui.drop_target);
                ui.drop_target = null_mut();
            }
            remove_tray(ui);
            if !ui.font.is_null() {
                DeleteObject(ui.font);
            }
            if !ui.details_font.is_null() {
                DeleteObject(ui.details_font);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

const MAIN_BUTTONS: [(i32, &str, &str); 5] = [
    (ID_ADD, "Yeni indirme", "New download"),
    (ID_SETTINGS, "Ayarlar", "Settings"),
    (ID_PAUSE_RESUME, "Duraklat / Sürdür", "Pause / Resume"),
    (ID_REMOVE, "Kaldır", "Remove"),
    (ID_OPEN_FOLDER, "Klasör", "Folder"),
];
const MAIN_FILTERS: [(&str, &str); 9] = [
    ("Tümü", "All"),
    ("Etkin", "Active"),
    ("Tamamlanan", "Completed"),
    ("Hatalar", "Errors"),
    ("Video", "Video"),
    ("Ses", "Audio"),
    ("Belgeler", "Documents"),
    ("Arşivler", "Archives"),
    ("Programlar", "Programs"),
];
const FILTER_VALUES: [Filter; 9] = [
    Filter::All,
    Filter::Active,
    Filter::Completed,
    Filter::Errors,
    Filter::Video,
    Filter::Audio,
    Filter::Documents,
    Filter::Archives,
    Filter::Programs,
];
const MAIN_COLUMNS: [(&str, &str, i32); 6] = [
    ("Ad", "Name", 260),
    ("Durum", "State", 145),
    ("İlerleme", "Progress", 170),
    ("Hız", "Speed", 100),
    ("Kalan", "Remaining", 85),
    ("Konum", "Location", 300),
];

unsafe fn create_main_controls(ui: &mut MainUi) {
    ui.dpi = GetDpiForWindow(ui.hwnd).max(96);
    ui.font = make_font(ui.dpi, 9, FW_NORMAL as i32);
    ui.details_font = make_font(ui.dpi, 9, FW_NORMAL as i32);
    ui.icon = {
        let own = LoadIconW(GetModuleHandleW(null()), 101usize as *const u16);
        if own.is_null() {
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            own
        }
    };
    SendMessageW(ui.hwnd, WM_SETICON, ICON_BIG as usize, ui.icon as isize);
    SendMessageW(ui.hwnd, WM_SETICON, ICON_SMALL as usize, ui.icon as isize);
    let button_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32;
    for (id, tr, en) in MAIN_BUTTONS {
        let h = control(
            ui.hwnd,
            "BUTTON",
            crate::i18n::ui(tr, en),
            button_style,
            0,
            id,
        );
        apply_font(h, ui.font);
        ui.toolbar.push(h);
    }
    let search_label = control(
        ui.hwnd,
        "STATIC",
        crate::i18n::ui("Ara:", "Search:"),
        WS_CHILD | WS_VISIBLE,
        0,
        0,
    );
    apply_font(search_label, ui.font);
    ui.toolbar.push(search_label);
    ui.search = control(
        ui.hwnd,
        "EDIT",
        "",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
        ID_SEARCH,
    );
    apply_font(ui.search, ui.font);
    ui.filter = control(
        ui.hwnd,
        "COMBOBOX",
        "",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
        0,
        ID_FILTER,
    );
    apply_font(ui.filter, ui.font);
    for (tr, en) in MAIN_FILTERS {
        combo_add(ui.filter, crate::i18n::ui(tr, en));
    }
    combo_select(ui.filter, 0);

    ui.list = control(
        ui.hwnd,
        "SysListView32",
        crate::i18n::ui("İndirme işleri", "Downloads"),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | LVS_REPORT | LVS_SHOWSELALWAYS,
        WS_EX_CLIENTEDGE,
        ID_JOB_LIST,
    );
    apply_font(ui.list, ui.font);
    SendMessageW(
        ui.list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_GRIDLINES | LVS_EX_LABELTIP) as isize,
    );
    for (index, (tr, en, width)) in MAIN_COLUMNS.iter().enumerate() {
        let mut name_w = wide(crate::i18n::ui(tr, en));
        let column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
            fmt: LVCFMT_LEFT,
            cx: scale(*width, ui.dpi),
            pszText: name_w.as_mut_ptr(),
            cchTextMax: name_w.len() as i32,
            iSubItem: index as i32,
            iImage: 0,
            iOrder: index as i32,
            cxMin: 0,
            cxDefault: 0,
            cxIdeal: 0,
        };
        SendMessageW(
            ui.list,
            LVM_INSERTCOLUMNW,
            index,
            &column as *const _ as isize,
        );
    }
    ui.details = control(
        ui.hwnd,
        "EDIT",
        crate::i18n::ui("Bir iş seçin.", "Select a job."),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_BORDER
            | ES_MULTILINE as u32
            | ES_READONLY as u32
            | ES_AUTOVSCROLL as u32
            | WS_VSCROLL,
        WS_EX_CLIENTEDGE,
        ID_DETAILS,
    );
    apply_font(ui.details, ui.details_font);
    // The list header paints itself in dark mode; the hook is installed here so
    // the first frame already matches the scheme.
    subclass(
        SendMessageW(ui.list, LVM_GETHEADER, 0, 0) as HWND,
        header_proc,
    );
    ui.status = CreateStatusWindowW(
        (WS_CHILD | WS_VISIBLE | SBARS_SIZEGRIP) as i32,
        wide(crate::i18n::ui("Hazır", "Ready")).as_ptr(),
        ui.hwnd,
        ID_STATUS as u32,
    );
    apply_font(ui.status, ui.font);
    subclass(ui.status, status_proc);
    // A menu bar measures its items with the menu font, so the bar has to carry
    // the same font the strip paints with; otherwise every caption outgrows its
    // item rect and the last letter is clipped.
    let menu = create_main_menu(ui.ui_mode, ui.debug_mode);
    SendMessageW(menu as HWND, WM_SETFONT, ui.font as usize, 1);
    SetMenu(ui.hwnd, menu);
    update_profile_controls(ui);

    DragAcceptFiles(ui.hwnd, 1);
    let target = Box::into_raw(Box::new(DropTarget {
        vtable: &DROP_TARGET_VTABLE,
        refs: AtomicU32::new(1),
        hwnd: ui.hwnd,
    }));
    if RegisterDragDrop(ui.hwnd, target.cast()) >= 0 {
        ui.drop_target = target;
    } else {
        drop_release(target);
    }
    AddClipboardFormatListener(ui.hwnd);
    add_tray(ui);
    SetTimer(ui.hwnd, TIMER_REFRESH, 350, None);
    apply_dark_mode(ui.hwnd, ui.snapshot.settings.dark_mode);
    layout_main(ui);
    refresh(ui, true);
}

/// Menu bar in the order standard Windows applications use. Mnemonics are
/// unique inside each menu; dialog openers keep the trailing ellipsis.
unsafe fn create_main_menu(mode: UiMode, debug: bool) -> HMENU {
    let bar = CreateMenu();
    let file = CreatePopupMenu();
    append(
        file,
        ID_ADD,
        crate::i18n::ui("&Yeni indirme...\tCtrl+N", "&New download...\tCtrl+N"),
    );
    append(
        file,
        ID_ANALYZE,
        crate::i18n::ui(
            "&Video indir...\tCtrl+Shift+N",
            "&Video download...\tCtrl+Shift+N",
        ),
    );
    append(
        file,
        ID_BATCH,
        crate::i18n::ui("&Toplu indirme...", "&Batch download..."),
    );
    AppendMenuW(file, MF_SEPARATOR, 0, null());
    append(
        file,
        ID_IMPORT_LIST,
        crate::i18n::ui("Listeyi &içe aktar...", "&Import list..."),
    );
    append(
        file,
        ID_EXPORT_LIST,
        crate::i18n::ui("Listeyi &dışa aktar...", "E&xport list..."),
    );
    AppendMenuW(file, MF_SEPARATOR, 0, null());
    append(file, ID_EXIT, crate::i18n::ui("Çı&kış", "E&xit"));
    AppendMenuW(
        bar,
        MF_POPUP,
        file as usize,
        wide(crate::i18n::ui("&Dosya", "&File")).as_ptr(),
    );
    let jobs = CreatePopupMenu();
    append(
        jobs,
        ID_PAUSE_RESUME,
        crate::i18n::ui(
            "Seçilenleri &duraklat / sürdür\tBoşluk",
            "&Pause / resume selected\tSpace",
        ),
    );
    append(
        jobs,
        ID_REMOVE,
        crate::i18n::ui("Seçilenleri ka&ldır\tDelete", "R&emove selected\tDelete"),
    );
    append(
        jobs,
        ID_PAUSE_ALL,
        crate::i18n::ui("Tümünü du&raklat", "Pau&se all"),
    );
    append(
        jobs,
        ID_RESUME_ALL,
        crate::i18n::ui("Tümünü &sürdür", "Re&sume all"),
    );
    append(
        jobs,
        ID_CLEAR_COMPLETED,
        crate::i18n::ui("&Tamamlananları temizle", "&Clear completed"),
    );
    append(
        jobs,
        ID_RETRY_FAILED,
        crate::i18n::ui("&Başarısızları yeniden dene", "Retry &failed"),
    );
    append(
        jobs,
        ID_MINI_PANEL,
        crate::i18n::ui("&Mini panel", "&Mini panel"),
    );
    AppendMenuW(jobs, MF_SEPARATOR, 0, null());
    append(
        jobs,
        ID_OPEN_FILE,
        crate::i18n::ui("Dosyayı a&ç\tEnter", "&Open file\tEnter"),
    );
    append(
        jobs,
        ID_OPEN_FOLDER,
        crate::i18n::ui("&Klasörü aç", "Open fol&der"),
    );
    append(
        jobs,
        ID_SOURCE_REFRESH,
        crate::i18n::ui("Kaynak adresini &yenile...", "&Refresh source address..."),
    );
    append(
        jobs,
        ID_SPEED_LIMIT,
        crate::i18n::ui("&Hız sınırı...", "Speed &limit..."),
    );
    AppendMenuW(jobs, MF_SEPARATOR, 0, null());
    append(
        jobs,
        ID_SELECT_ALL,
        crate::i18n::ui("Tümün&ü seç\tCtrl+A", "Select &all\tCtrl+A"),
    );
    AppendMenuW(
        bar,
        MF_POPUP,
        jobs as usize,
        wide(crate::i18n::ui("İ&ndirme", "&Downloads")).as_ptr(),
    );
    let tools = CreatePopupMenu();
    if !mode.is_simple() {
        append(
            tools,
            ID_QUEUE_MANAGER,
            crate::i18n::ui("&Kuyruk yöneticisi...", "&Queue manager..."),
        );
        append(
            tools,
            ID_FOLDER_RULES,
            crate::i18n::ui("Klasör k&uralları...", "Folder &rules..."),
        );
        append(
            tools,
            ID_SITE_CRAWLER,
            crate::i18n::ui("Site &gezgini...", "Site e&xplorer..."),
        );
        append(
            tools,
            ID_SYNC_MANAGER,
            crate::i18n::ui("&Eşitleme yöneticisi...", "Syn&c manager..."),
        );
        append(
            tools,
            ID_SITE_LOGINS,
            crate::i18n::ui("Site &girişleri...", "Site lo&gins..."),
        );
        AppendMenuW(tools, MF_SEPARATOR, 0, null());
        append(
            tools,
            ID_EVENT_LOG,
            crate::i18n::ui("&Olay günlüğü...", "&Event log..."),
        );
        append(
            tools,
            ID_JOB_LOG,
            crate::i18n::ui("İş günl&üğü...", "Job &log..."),
        );
    }
    // The media-tools surface is a developer/repair surface: it names the
    // external media tools, so it only exists in developer mode.
    if debug {
        append(
            tools,
            ID_TOOLS,
            crate::i18n::ui("&Medya araçları...", "&Media tools..."),
        );
    }
    append(
        tools,
        ID_INSTALL_TOOLS,
        crate::i18n::ui("Medya desteğini &kur...", "Install media &support..."),
    );
    append(
        tools,
        ID_BROWSER_SETUP,
        crate::i18n::ui("&Tarayıcı kurulumu...", "&Browser setup..."),
    );
    AppendMenuW(tools, MF_SEPARATOR, 0, null());
    append(
        tools,
        ID_SETTINGS,
        crate::i18n::ui("&Ayarlar...", "&Settings..."),
    );
    if debug || !mode.is_simple() {
        append(
            tools,
            ID_DIAGNOSTICS,
            crate::i18n::ui("Ta&nılama...", "&Diagnostics..."),
        );
    }
    AppendMenuW(
        bar,
        MF_POPUP,
        tools as usize,
        wide(crate::i18n::ui("A&raçlar", "&Tools")).as_ptr(),
    );
    let help = CreatePopupMenu();
    append(
        help,
        ID_UPDATE_CHECK,
        crate::i18n::ui("&Güncellemeleri denetle...", "&Check for updates..."),
    );
    if let Some(version) = crate::update::available_version() {
        append(
            help,
            ID_INSTALL_UPDATE,
            &crate::i18n::ui_owned!(
                format!("Güncellemeyi &kur ({version})..."),
                format!("&Install update ({version})...")
            ),
        );
    }
    AppendMenuW(help, MF_SEPARATOR, 0, null());
    append(
        help,
        1401,
        crate::i18n::ui("&Klavye kısayolları", "&Keyboard shortcuts"),
    );
    append(
        help,
        ID_SHOW_TOUR,
        crate::i18n::ui("&Tanıtımı göster", "Show the &tour"),
    );
    append(
        help,
        1402,
        crate::i18n::ui("SSDownload &hakkında", "SSDownload &about"),
    );
    AppendMenuW(
        bar,
        MF_POPUP,
        help as usize,
        wide(crate::i18n::ui("&Yardım", "&Help")).as_ptr(),
    );
    bar
}
unsafe fn append(menu: HMENU, id: i32, label: &str) {
    AppendMenuW(menu, MF_STRING, id as usize, wide(label).as_ptr());
}

/// Rebuilds the menu bar when the surface depth or developer mode changed. The
/// bar is the only structure the mode hides, so it is replaced as a whole
/// instead of mutated item by item, and the previous handle is destroyed.
unsafe fn apply_menu_mode(ui: &mut MainUi, snapshot: &AppSnapshot) {
    let mode = UiMode::parse(&snapshot.settings.ui_mode);
    let debug = snapshot.settings.debug_mode;
    let language_changed = snapshot.settings.ui_language != ui.snapshot.settings.ui_language;
    // Developer mode owns the first-run flow: a run that asked for it never
    // has a wizard pending, whichever profile it opens.
    ui.wizard_pending = snapshot.settings.onboarding_version < ONBOARDING_VERSION && !debug;
    if mode == ui.ui_mode && debug == ui.debug_mode && !language_changed {
        return;
    }
    ui.ui_mode = mode;
    ui.debug_mode = debug;
    let menu = create_main_menu(mode, debug);
    SendMessageW(menu as HWND, WM_SETFONT, ui.font as usize, 1);
    let previous = GetMenu(ui.hwnd);
    SetMenu(ui.hwnd, menu);
    if !previous.is_null() {
        DestroyMenu(previous);
    }
    if language_changed {
        set_text(
            ui.hwnd,
            crate::i18n::ui(APP_TITLE, "SSDownload — Download Manager"),
        );
        for (id, tr, en) in MAIN_BUTTONS {
            set_text(GetDlgItem(ui.hwnd, id), crate::i18n::ui(tr, en));
        }
        if let Some(label) = ui.toolbar.get(5) {
            set_text(*label, crate::i18n::ui("Ara:", "Search:"));
        }
        let filter = combo_index(ui.filter);
        SendMessageW(ui.filter, CB_RESETCONTENT, 0, 0);
        for (tr, en) in MAIN_FILTERS {
            combo_add(ui.filter, crate::i18n::ui(tr, en));
        }
        combo_select(ui.filter, filter);
        for (index, (tr, en, _)) in MAIN_COLUMNS.iter().enumerate() {
            let mut label = wide(crate::i18n::ui(tr, en));
            let column = LVCOLUMNW {
                mask: LVCF_TEXT,
                pszText: label.as_mut_ptr(),
                ..zeroed()
            };
            SendMessageW(ui.list, LVM_SETCOLUMNW, index, &column as *const _ as isize);
        }
        set_text(ui.list, crate::i18n::ui("İndirme işleri", "Downloads"));
        ui.row_cache.clear();
        layout_main(ui);
        DrawMenuBar(ui.hwnd);
        InvalidateRect(ui.hwnd, null(), 1);
    }
    update_profile_controls(ui);
}

/// Keeps the user-facing retry usable exactly while it can do something: a
/// component is missing or the last attempt reported a failure, and no install
/// is running. Called on every tick so the state follows the snapshot.
unsafe fn update_tools_menu(ui: &MainUi, snapshot: &AppSnapshot) {
    let usable = !snapshot.installing_tools
        && (snapshot.tools_error.is_some() || snapshot.tools.iter().any(|tool| !tool.installed));
    EnableMenuItem(
        GetMenu(ui.hwnd),
        ID_INSTALL_TOOLS as u32,
        MF_BYCOMMAND | if usable { MF_ENABLED } else { MF_GRAYED },
    );
}

unsafe fn update_profile_controls(ui: &MainUi) {
    let modes = ui.snapshot.settings.usage_modes;
    let menu = GetMenu(ui.hwnd);
    let media_enabled = modes.allows_inspection();
    let label = if modes.video {
        crate::i18n::ui(
            "&Video indir...\tCtrl+Shift+N",
            "&Video download...\tCtrl+Shift+N",
        )
    } else {
        crate::i18n::ui(
            "&Ses indir...\tCtrl+Shift+N",
            "&Audio download...\tCtrl+Shift+N",
        )
    };
    ModifyMenuW(
        menu,
        ID_ANALYZE as u32,
        MF_BYCOMMAND | MF_STRING,
        ID_ANALYZE as usize,
        wide(label).as_ptr(),
    );
    EnableMenuItem(
        menu,
        ID_ANALYZE as u32,
        MF_BYCOMMAND | if media_enabled { MF_ENABLED } else { MF_GRAYED },
    );
    EnableMenuItem(
        menu,
        ID_TOOLS as u32,
        MF_BYCOMMAND | if media_enabled { MF_ENABLED } else { MF_GRAYED },
    );
    for (id, enabled) in [
        (ID_QUEUE_MANAGER, ui.management_capable),
        (ID_FOLDER_RULES, ui.management_capable && modes.file),
        (ID_SITE_CRAWLER, ui.management_capable && modes.file),
        (ID_SYNC_MANAGER, ui.management_capable && modes.file),
    ] {
        EnableMenuItem(
            menu,
            id as u32,
            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
        );
    }
    DrawMenuBar(ui.hwnd);
}

/// A window minimum is bound by the space the monitor actually offers: at high
/// scaling a 700x470 design does not fit a 1080p work area, and a minimum the
/// window could not shrink below would push the title bar and the buttons out
/// of reach. A small margin keeps the frame grabbable.
unsafe fn clamp_min_track(hwnd: HWND, dpi: u32, design_w: i32, design_h: i32) -> (i32, i32) {
    let mut monitor: MONITORINFO = zeroed();
    monitor.cbSize = size_of::<MONITORINFO>() as u32;
    let from = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let (mut area_w, mut area_h) = (i32::MAX, i32::MAX);
    if !from.is_null() && GetMonitorInfoW(from, &mut monitor) != 0 {
        area_w = monitor.rcWork.right - monitor.rcWork.left;
        area_h = monitor.rcWork.bottom - monitor.rcWork.top;
    }
    let margin = scale(16, dpi);
    (
        scale(design_w, dpi).min((area_w - margin).max(scale(200, dpi))),
        scale(design_h, dpi).min((area_h - margin).max(scale(150, dpi))),
    )
}

unsafe fn update_main_dpi(ui: &mut MainUi) {
    let old_dpi = ui.dpi;
    ui.dpi = GetDpiForWindow(ui.hwnd).max(96);
    for col in 0..6 {
        let width = SendMessageW(ui.list, LVM_GETCOLUMNWIDTH, col, 0);
        SendMessageW(
            ui.list,
            LVM_SETCOLUMNWIDTH,
            col,
            width * ui.dpi as isize / old_dpi.max(96) as isize,
        );
    }
    let new_font = make_font(ui.dpi, 9, FW_NORMAL as i32);
    let new_details = make_font(ui.dpi, 9, FW_NORMAL as i32);
    for &h in &ui.toolbar {
        apply_font(h, new_font);
    }
    for h in [ui.search, ui.filter, ui.list, ui.status] {
        apply_font(h, new_font);
    }
    apply_font(ui.details, new_details);
    DeleteObject(ui.font);
    DeleteObject(ui.details_font);
    ui.font = new_font;
    ui.details_font = new_details;
    // The bar measures its items with the menu font, so the items have to be
    // re-measured with the new font before the strip paints with it.
    let menu = GetMenu(ui.hwnd);
    if !menu.is_null() {
        SendMessageW(menu as HWND, WM_SETFONT, ui.font as usize, 1);
    }
    layout_main(ui);
    DrawMenuBar(ui.hwnd);
}

unsafe fn layout_main(ui: &MainUi) {
    let mut rect: RECT = zeroed();
    GetClientRect(ui.hwnd, &mut rect);
    let w = rect.right.max(scale(640, ui.dpi));
    let h = rect.bottom;
    let m = scale(10, ui.dpi);
    let top = scale(10, ui.dpi);
    let bh = scale(32, ui.dpi);
    let gap = scale(6, ui.dpi);
    let widths = [
        scale(112, ui.dpi),
        scale(80, ui.dpi),
        scale(142, ui.dpi),
        scale(76, ui.dpi),
        scale(78, ui.dpi),
    ];
    let mut x = m;
    for (i, &button) in ui.toolbar.iter().take(5).enumerate() {
        MoveWindow(button, x, top, widths[i], bh, 1);
        x += widths[i] + gap;
    }
    x = m;
    let search_top = top + bh + gap;
    let filter_w = scale(125, ui.dpi);
    let label = ui.toolbar[5];
    // "Search:" is wider than "Ara:", so the label follows its own text.
    let label_w = (text_width(label) + scale(2, ui.dpi)).max(scale(30, ui.dpi));
    let search_w = (w - x - filter_w - m * 2 - label_w - scale(6, ui.dpi)).max(scale(120, ui.dpi));
    MoveWindow(
        label,
        x,
        search_top + scale(8, ui.dpi),
        label_w,
        scale(20, ui.dpi),
        1,
    );
    x += label_w + scale(4, ui.dpi);
    MoveWindow(
        ui.search,
        x,
        search_top + scale(2, ui.dpi),
        search_w,
        scale(27, ui.dpi),
        1,
    );
    MoveWindow(
        ui.filter,
        x + search_w + gap,
        search_top + scale(2, ui.dpi),
        filter_w,
        scale(240, ui.dpi),
        1,
    );
    let status_h = scale(24, ui.dpi);
    let details_h = (h / 4).clamp(scale(130, ui.dpi), scale(210, ui.dpi));
    let list_top = search_top + bh + scale(10, ui.dpi);
    let details_y = h - status_h - details_h - m;
    MoveWindow(
        ui.list,
        m,
        list_top,
        w - 2 * m,
        (details_y - list_top - gap).max(scale(100, ui.dpi)),
        1,
    );
    MoveWindow(ui.details, m, details_y, w - 2 * m, details_h, 1);
    SendMessageW(ui.status, WM_SIZE, 0, 0);
}

/// Whether the current surface offers a main-window command. Mirrors
/// `create_main_menu` so a hidden item is also refused when something other
/// than the bar reaches it.
fn main_command_visible(mode: UiMode, debug: bool, id: i32) -> bool {
    match id {
        ID_TOOLS => debug,
        ID_DIAGNOSTICS => debug || !mode.is_simple(),
        ID_QUEUE_MANAGER | ID_FOLDER_RULES | ID_SITE_CRAWLER | ID_SYNC_MANAGER | ID_EVENT_LOG
        | ID_JOB_LOG | ID_SITE_LOGINS => !mode.is_simple(),
        _ => true,
    }
}

unsafe fn main_command(ui: &mut MainUi, id: i32, notify_code: u16) {
    // A surface the current mode does not show is refused here as well: an
    // accelerator or a stale message must not open a window the menu hides.
    if !main_command_visible(ui.ui_mode, ui.debug_mode, id) {
        return;
    }
    match id {
        ID_ADD => show_add_dialog(
            ui.hwnd,
            ui.app.clone(),
            clipboard_urls().join("\r\n"),
            false,
        ),
        ID_ANALYZE if ui.snapshot.settings.usage_modes.allows_inspection() => {
            show_add_dialog(ui.hwnd, ui.app.clone(), clipboard_urls().join("\r\n"), true)
        }
        ID_TOOLS if ui.snapshot.settings.usage_modes.allows_inspection() => {
            show_tools_dialog(ui.hwnd, ui.app.clone())
        }
        ID_INSTALL_TOOLS => {
            if let Err(error) = ui.app.dispatch_result(Action::InstallTools {
                request_id: None,
                force_update: false,
            }) {
                ui.app.message(
                    format!(
                        "{}: {error:#}",
                        crate::i18n::ui(
                            "Medya desteği kurulumu başlatılamadı",
                            "Media support installation could not be started",
                        )
                    ),
                    true,
                );
            }
        }
        ID_PAUSE_RESUME => pause_resume_selected(ui),
        ID_REMOVE => remove_selected(ui),
        ID_SELECT_ALL => select_all_jobs(ui),
        ID_OPEN_FOLDER => open_selected_folder(ui),
        ID_OPEN_FILE => open_selected_file(ui),
        ID_SOURCE_REFRESH => begin_source_refresh(ui),
        ID_PAUSE_ALL => dispatch(ui, Action::PauseAll),
        ID_RESUME_ALL => dispatch(ui, Action::ResumeAll),
        ID_CLEAR_COMPLETED => dispatch(ui, Action::ClearCompleted),
        ID_BATCH => show_add_dialog(
            ui.hwnd,
            ui.app.clone(),
            "https://example.com/dosya[001-010].zip".into(),
            false,
        ),
        ID_IMPORT_LIST => import_url_list(ui),
        ID_EXPORT_LIST => export_url_list(ui),
        ID_SPEED_LIMIT => {
            let ids = selected_ids(ui);
            if ids.is_empty() {
                message(
                    ui.hwnd,
                    crate::i18n::ui(
                        "Önce listeden bir veya daha fazla indirme seçin.",
                        "Select one or more downloads in the list first.",
                    ),
                    crate::i18n::ui("Hız sınırı", "Speed limit"),
                    MB_OK | MB_ICONINFORMATION,
                );
            } else {
                let initial = ui
                    .snapshot
                    .jobs
                    .iter()
                    .find(|job| job.id == ids[0])
                    .and_then(|job| job.request.speed_limit_kib)
                    .unwrap_or(0);
                show_speed_limit(ui.hwnd, ui.app.clone(), ids, initial);
            }
        }
        ID_SITE_LOGINS if ui.management_capable => show_site_logins(ui.hwnd, ui.app.clone()),
        ID_RETRY_FAILED => dispatch(ui, Action::RetryFailed),
        ID_MINI_PANEL => open_card(ui.hwnd, ui.app.clone(), CardKind::Panel),
        ID_INSTALL_UPDATE => crate::update::check_now(ui.app.clone()),
        ID_COPY_URL => {
            let urls = selected_ids(ui)
                .iter()
                .filter_map(|id| ui.snapshot.jobs.iter().find(|job| &job.id == id))
                .map(|job| job.request.url.clone())
                .collect::<Vec<_>>();
            if !urls.is_empty() && set_clipboard_text(&urls.join("\r\n")) {
                set_text(
                    ui.status,
                    crate::i18n::ui(
                        "Adres panoya kopyalandı.",
                        "Address copied to the clipboard.",
                    ),
                );
            }
        }
        ID_REDOWNLOAD => {
            for id in selected_ids(ui) {
                if let Some(job) = ui.snapshot.jobs.iter().find(|job| job.id == id) {
                    let mut request = job.request.idempotency_view();
                    request.request_id = None;
                    request.directory = job.path.parent().map(Path::to_path_buf);
                    dispatch(ui, Action::Add { request });
                }
            }
        }
        ID_RENAME => {
            if let Some(job) = selected_id(ui)
                .and_then(|id| ui.snapshot.jobs.iter().find(|job| job.id == id).cloned())
            {
                show_rename(ui.hwnd, ui.app.clone(), job.id.clone(), job.name.clone());
            }
        }
        ID_MOVE_TOP | ID_MOVE_BOTTOM => {
            let ids = selected_ids(ui);
            if !ids.is_empty() {
                dispatch(
                    ui,
                    Action::Reorder {
                        ids,
                        top: id == ID_MOVE_TOP,
                    },
                );
            }
        }
        ID_START_NOW => {
            for id in selected_ids(ui) {
                dispatch(ui, Action::StartNow { id });
            }
        }
        ID_OPEN_WHEN_DONE => {
            if let Some(job) = selected_id(ui)
                .and_then(|id| ui.snapshot.jobs.iter().find(|job| job.id == id).cloned())
            {
                dispatch(
                    ui,
                    Action::SetOpenWhenDone {
                        id: job.id.clone(),
                        value: !job.open_when_done,
                    },
                );
            }
        }
        id if (ID_QUEUE_BASE..ID_QUEUE_BASE + 64).contains(&id) => {
            let index = (id - ID_QUEUE_BASE) as usize;
            if let Some(queue) = ui.snapshot.settings.queues.get(index) {
                let ids = selected_ids(ui);
                if !ids.is_empty() {
                    dispatch(
                        ui,
                        Action::MoveToQueue {
                            ids,
                            queue_id: queue.id.clone(),
                        },
                    );
                }
            }
        }
        ID_TRAY_ADD => {
            show_main(ui);
            show_add_dialog(ui.hwnd, ui.app.clone(), String::new(), false);
        }
        ID_TRAY_CLIPBOARD => show_add_dialog(
            ui.hwnd,
            ui.app.clone(),
            clipboard_urls().join("\r\n"),
            false,
        ),
        ID_TRAY_SPEED_UNLIMITED | ID_TRAY_SPEED_1M | ID_TRAY_SPEED_512K => {
            let mut settings = ui.app.snapshot().settings;
            settings.speed_limit_kib = match id {
                ID_TRAY_SPEED_1M => 1024,
                ID_TRAY_SPEED_512K => 512,
                _ => 0,
            };
            let _ = ui.app.update_settings_quiet(settings);
        }
        ID_TRAY_SPEED_CUSTOM => {
            let current = ui.app.snapshot().settings.speed_limit_kib;
            show_speed_limit(ui.hwnd, ui.app.clone(), Vec::new(), current);
        }
        ID_TRAY_SHUTDOWN => {
            let snapshot = ui.app.snapshot();
            if let Some(mut queue) = snapshot
                .settings
                .queues
                .iter()
                .find(|queue| queue.id == crate::model::DEFAULT_QUEUE_ID)
                .cloned()
            {
                queue.completion =
                    if matches!(queue.completion, CompletionAction::ShutdownComputer { .. }) {
                        CompletionAction::None
                    } else {
                        CompletionAction::ShutdownComputer {
                            countdown_seconds: 60,
                        }
                    };
                dispatch(ui, Action::UpdateQueuePolicy { queue });
            }
        }
        ID_TRAY_FOLDER => {
            let folder = ui.snapshot.settings.download_dir.clone();
            open_folder(&folder);
        }
        ID_TRAY_QUIET => {
            ui.quiet_until = if ui.quiet_until.is_some_and(|until| until > Instant::now()) {
                None
            } else {
                Some(Instant::now() + Duration::from_secs(3600))
            };
        }
        ID_SETTINGS => {
            // Settings save synchronously commits through the actor, while the painted main
            // snapshot is normally refreshed on a timer. Opening the next modal must read the
            // committed snapshot now rather than resurrecting that stale timer snapshot.
            let snapshot = ui.app.snapshot();
            ui.snapshot = snapshot.clone();
            show_settings_dialog(ui.hwnd, ui.app.clone(), snapshot.settings);
        }
        ID_QUEUE_MANAGER if ui.management_capable => {
            show_queue_manager(ui.hwnd, ui.app.clone(), selected_id(ui))
        }
        ID_EVENT_LOG => show_event_log(ui.hwnd, ui.app.clone()),
        ID_JOB_LOG => show_selected_job_log(ui),
        ID_UPDATE_CHECK => crate::update::check_now(ui.app.clone()),
        ID_FOLDER_RULES if ui.management_capable && ui.snapshot.settings.usage_modes.file => {
            show_rule_editor(ui.hwnd, ui.app.clone())
        }
        ID_SITE_CRAWLER if ui.management_capable && ui.snapshot.settings.usage_modes.file => {
            show_crawler(ui.hwnd, ui.app.clone())
        }
        ID_SYNC_MANAGER if ui.management_capable && ui.snapshot.settings.usage_modes.file => {
            show_sync_manager(ui.hwnd, ui.app.clone())
        }
        ID_BROWSER_SETUP => dispatch(ui, Action::BrowserSetup),
        ID_DIAGNOSTICS => dispatch(ui, Action::ExportDiagnostics),
        ID_EXIT => exit_app(ui),
        ID_SEARCH if notify_code as u32 == EN_CHANGE => refresh(ui, true),
        ID_FILTER if notify_code as u32 == CBN_SELCHANGE => {
            ui.filter_value = FILTER_VALUES
                .get(combo_index(ui.filter).max(0) as usize)
                .copied()
                .unwrap_or(Filter::All);
            refresh(ui, true);
        }
        1401 => {
            message(
                ui.hwnd,
                crate::i18n::ui(
                    "Ctrl+N  Yeni indirme\nCtrl+Shift+N  Video indir\nCtrl+V  Panodaki URL'leri ekle\nCtrl+F  Arama\nCtrl+A  Tümünü seç\nDelete  Seçilenleri kaldır\nF5  Yenile\nEnter  Dosyayı aç\nBoşluk  Duraklat / sürdür\nAlt+Home / Alt+End  En üste / en alta taşı\nCtrl+Shift+D  Panodaki bağlantıyı her yerden ekle",
                    "Ctrl+N  New download\nCtrl+Shift+N  Download video\nCtrl+V  Add URLs from the clipboard\nCtrl+F  Search\nCtrl+A  Select all\nDelete  Remove selected\nF5  Refresh\nEnter  Open file\nSpace  Pause / resume\nAlt+Home / Alt+End  Move to top / bottom\nCtrl+Shift+D  Add the clipboard link from anywhere",
                ),
                crate::i18n::ui("Klavye kısayolları", "Keyboard shortcuts"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        1402 => show_about(ui),
        ID_SHOW_TOUR => show_tour(ui, true),
        1403 => {
            if GetFocus() == ui.search {
                SendMessageW(ui.search, WM_PASTE, 0, 0);
            } else {
                let urls = clipboard_urls();
                if urls.is_empty() {
                    message(
                        ui.hwnd,
                        crate::i18n::ui(
                            "Panoda geçerli bir HTTP/HTTPS bağlantısı yok.",
                            "The clipboard does not contain a valid HTTP/HTTPS link.",
                        ),
                        crate::i18n::ui("Panodan ekle", "Add from clipboard"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                } else {
                    show_add_dialog(ui.hwnd, ui.app.clone(), urls.join("\r\n"), false);
                }
            }
        }
        1404 => {
            SetFocus(ui.search);
            SendMessageW(ui.search, EM_SETSEL, 0, -1);
        }
        1405 => refresh(ui, true),
        ID_TRAY_SHOW => show_main(ui),
        ID_TRAY_PAUSE => dispatch(ui, Action::PauseAll),
        ID_TRAY_RESUME => dispatch(ui, Action::ResumeAll),
        _ => {}
    }
}

/// Age after which the media toolchain is refreshed at startup.
const TOOLCHAIN_REFRESH_DAYS: i64 = 7;

/// "SSDownload 1.5.1 — 2 indiriliyor · 5,2 MB/sn" in the title and the tray tooltip,
/// plus a progress bar on the tray icon; each is rewritten only when it changes.
unsafe fn update_title_and_tray(ui: &mut MainUi) {
    let running = ui
        .snapshot
        .jobs
        .iter()
        .filter(|job| {
            matches!(
                job.state,
                JobState::Downloading | JobState::Processing | JobState::Connecting
            )
        })
        .collect::<Vec<_>>();
    let speed: u64 = running.iter().map(|job| job.speed).sum();
    let version = env!("CARGO_PKG_VERSION");
    let update = crate::update::available_version()
        .filter(|candidate| crate::update::is_newer(version, candidate))
        .map(|candidate| {
            crate::i18n::ui_owned!(
                format!(" · Güncelleme var ({candidate})"),
                format!(" · Update available ({candidate})")
            )
        })
        .unwrap_or_default();
    let summary = if running.is_empty() {
        crate::i18n::ui("İndirme Yöneticisi", "Download Manager").to_string()
    } else {
        crate::i18n::ui_owned!(
            format!("{} indiriliyor · {}", running.len(), format_speed(speed)),
            format!("{} downloading · {}", running.len(), format_speed(speed))
        )
    };
    let title = format!("SSDownload {version} — {summary}{update}");
    if title != ui.title_text {
        SetWindowTextW(ui.hwnd, wide(&title).as_ptr());
        ui.title_text = title;
    }
    if !ui.tray_added {
        return;
    }
    let known = running
        .iter()
        .all(|job| job.total.is_some_and(|total| total > 0));
    let fraction = if running.is_empty() || !known {
        None
    } else {
        let total: u64 = running.iter().map(|job| job.total.unwrap_or(0)).sum();
        let done: u64 = running
            .iter()
            .map(|job| job.downloaded.min(job.total.unwrap_or(0)))
            .sum();
        Some(done as f64 / total.max(1) as f64)
    };
    let tip = if running.is_empty() {
        "SSDownload".to_string()
    } else {
        match fraction {
            Some(value) => format!("SSDownload · {summary} · %{}", (value * 100.0) as u32),
            None => format!("SSDownload · {summary}"),
        }
    };
    let bucket = fraction.map(|value| (value * 20.0) as i32).unwrap_or(-1);
    let key = format!("{tip}|{bucket}|{}", running.is_empty());
    if key == ui.tray_key {
        return;
    }
    ui.tray_key = key;
    let icon = if running.is_empty() {
        ui.icon
    } else {
        icon_with_progress(ui.icon, fraction.unwrap_or(0.0), fraction.is_none())
    };
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = ui.hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_TIP | NIF_ICON | NIF_SHOWTIP;
    data.hIcon = icon;
    copy_wide(&mut data.szTip, &tip);
    Shell_NotifyIconW(NIM_MODIFY, &data);
    if !ui.tray_icon.is_null() {
        DestroyIcon(ui.tray_icon);
    }
    ui.tray_icon = if icon == ui.icon { null_mut() } else { icon };
}

/// The application icon at tray size with a progress bar along its bottom edge.
unsafe fn icon_with_progress(base: HICON, fraction: f64, indeterminate: bool) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);
    let scaled = CopyImage(base as HANDLE, IMAGE_ICON, size, size, 0) as HICON;
    let source = if scaled.is_null() { base } else { scaled };
    let mut info: ICONINFO = zeroed();
    if GetIconInfo(source, &mut info) == 0 {
        return base;
    }
    let dc = GetDC(null_mut());
    let mut header: BITMAPINFO = zeroed();
    header.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    header.bmiHeader.biWidth = size;
    header.bmiHeader.biHeight = -size;
    header.bmiHeader.biPlanes = 1;
    header.bmiHeader.biBitCount = 32;
    header.bmiHeader.biCompression = BI_RGB;
    let mut pixels = vec![0u32; (size * size) as usize];
    let read = GetDIBits(
        dc,
        info.hbmColor,
        0,
        size as u32,
        pixels.as_mut_ptr().cast(),
        &mut header,
        DIB_RGB_COLORS,
    );
    if read == 0 {
        ReleaseDC(null_mut(), dc);
        DeleteObject(info.hbmColor as HGDIOBJ);
        DeleteObject(info.hbmMask as HGDIOBJ);
        return base;
    }
    // An icon without alpha takes full opacity where it has colour.
    if pixels.iter().all(|pixel| pixel >> 24 == 0) {
        for pixel in &mut pixels {
            if *pixel & 0x00ff_ffff != 0 {
                *pixel |= 0xff00_0000;
            }
        }
    }
    let bar = (size / 5).max(3);
    let filled = if indeterminate {
        size
    } else {
        ((size as f64) * fraction.clamp(0.0, 1.0)).round() as i32
    };
    for y in size - bar..size {
        for x in 0..size {
            let colour = if x < filled { 0xff00_71e3 } else { 0xff3a_3a3c };
            pixels[(y * size + x) as usize] = colour;
        }
    }
    let colour_bitmap = CreateDIBSection(dc, &header, DIB_RGB_COLORS, null_mut(), null_mut(), 0);
    ReleaseDC(null_mut(), dc);
    let result = if colour_bitmap.is_null() {
        base
    } else {
        let memory = CreateCompatibleDC(null_mut());
        SetDIBits(
            memory,
            colour_bitmap,
            0,
            size as u32,
            pixels.as_ptr().cast(),
            &header,
            DIB_RGB_COLORS,
        );
        DeleteDC(memory);
        let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, mask_bits.as_ptr().cast());
        let icon_info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: colour_bitmap,
        };
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(mask as HGDIOBJ);
        DeleteObject(colour_bitmap as HGDIOBJ);
        if icon.is_null() {
            base
        } else {
            icon
        }
    };
    DeleteObject(info.hbmColor as HGDIOBJ);
    DeleteObject(info.hbmMask as HGDIOBJ);
    if !scaled.is_null() {
        DestroyIcon(scaled);
    }
    result
}

/// Ctrl+Shift+D follows `Settings.global_hotkey`; a key another program owns is skipped.
unsafe fn sync_hotkey(ui: &mut MainUi) {
    let wanted = ui.snapshot.settings.global_hotkey;
    if wanted == ui.hotkey_registered || (wanted && ui.hotkey_failed) {
        return;
    }
    if wanted {
        ui.hotkey_registered = RegisterHotKey(
            ui.hwnd,
            HOTKEY_ADD,
            MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
            b'D' as u32,
        ) != 0;
        // A refusal is not retried every tick: the key stays with its owner.
        ui.hotkey_failed = !ui.hotkey_registered;
    } else {
        UnregisterHotKey(ui.hwnd, HOTKEY_ADD);
        ui.hotkey_registered = false;
    }
}

/// Filter entries carry their counts ("Video (3)"); rewritten only when a count changes.
unsafe fn update_filter_labels(ui: &mut MainUi) {
    let counts = FILTER_VALUES
        .iter()
        .map(|filter| {
            ui.snapshot
                .jobs
                .iter()
                .filter(|job| matches_filter(job, *filter))
                .count()
        })
        .collect::<Vec<_>>();
    if counts == ui.filter_counts {
        return;
    }
    ui.filter_counts = counts.clone();
    let selected = combo_index(ui.filter);
    SendMessageW(ui.filter, CB_RESETCONTENT, 0, 0);
    for ((tr, en), count) in MAIN_FILTERS.iter().zip(counts) {
        combo_add(ui.filter, &format!("{} ({count})", crate::i18n::ui(tr, en)));
    }
    combo_select(ui.filter, selected.max(0));
}

/// Restores the saved main window rectangle when it is still on a monitor; returns
/// whether the window was maximized.
unsafe fn restore_placement(hwnd: HWND, settings: &Settings) -> bool {
    let Some([left, top, right, bottom, maximized]) = settings.window_placement else {
        return false;
    };
    let rect = RECT {
        left,
        top,
        right,
        bottom,
    };
    if right - left < 400
        || bottom - top < 300
        || MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_null()
    {
        return false;
    }
    SetWindowPos(
        hwnd,
        null_mut(),
        left,
        top,
        right - left,
        bottom - top,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    maximized != 0
}

unsafe fn save_placement(ui: &MainUi) {
    let mut placement: WINDOWPLACEMENT = zeroed();
    placement.length = size_of::<WINDOWPLACEMENT>() as u32;
    if GetWindowPlacement(ui.hwnd, &mut placement) == 0 {
        return;
    }
    let rect = placement.rcNormalPosition;
    let value = [
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        i32::from(placement.showCmd == SW_SHOWMAXIMIZED as u32),
    ];
    let mut settings = ui.app.snapshot().settings;
    if settings.window_placement != Some(value) {
        settings.window_placement = Some(value);
        let _ = ui.app.update_settings_quiet(settings);
    }
}

/// Three short screens after the first start: the list, adding downloads, the tray.
/// The first-run tour; `replay` (Yardım menu) shows it even after it was seen.
unsafe fn show_tour(ui: &mut MainUi, replay: bool) {
    let snapshot = ui.app.snapshot();
    if snapshot.settings.tour_seen && !replay {
        return;
    }
    let steps = [
        crate::i18n::ui(
            "Burası indirme listesi. Satıra çift tıklayınca dosya açılır; sağ tık menüsünde duraklatma, öne alma, adres kopyalama ve daha fazlası var. Satırları sürükleyerek sırayı değiştirebilirsin.",
            "This is the download list. Double-click a row to open the file; the right-click menu has pause, move up, copy address and more. Drag rows to change their order.",
        ),
        crate::i18n::ui(
            "Yeni indirme için Ctrl+N'ye bas ya da bir bağlantıyı pencereye sürükle. Uygulama arka plandayken bile Ctrl+Shift+D panodaki bağlantıyı ekler. Tarayıcıda videonun üstündeki SSDownload düğmesi kaliteyi seçtirir.",
            "Press Ctrl+N for a new download or drop a link on the window. Even in the background, Ctrl+Shift+D adds the clipboard's link. In the browser, the SSDownload button over a video lets you pick the quality.",
        ),
        crate::i18n::ui(
            "Sağ alttaki simgeye sağ tıkla: hız sınırı, bitince kapatma, sessiz mod ve mini panel oradadır. Ayarların Ağ ve Davranış sekmeleri proxy, uyku engeli ve bildirimleri yönetir.",
            "Right-click the icon at the bottom right: speed limit, shut down when done, quiet mode and the mini panel live there. The Network and Behaviour settings tabs manage proxy, sleep prevention and notifications.",
        ),
    ];
    for (index, body) in steps.iter().enumerate() {
        let last = index + 1 == steps.len();
        let buttons: Vec<(i32, &'static str)> = if last {
            vec![(IDOK, crate::i18n::ui("Başla", "Start"))]
        } else {
            vec![
                (IDOK, crate::i18n::ui("İleri", "Next")),
                (IDCANCEL, crate::i18n::ui("Atla", "Skip")),
            ]
        };
        let title = crate::i18n::ui_owned!(
            format!("SSDownload turu ({}/{})", index + 1, steps.len()),
            format!("SSDownload tour ({}/{})", index + 1, steps.len())
        );
        let answer = run_alert_buttons(ui.hwnd, body, &title, MB_ICONINFORMATION, Some(&buttons))
            .unwrap_or(IDCANCEL);
        if answer != IDOK {
            break;
        }
    }
    if !snapshot.settings.tour_seen {
        let mut settings = ui.app.snapshot().settings;
        settings.tour_seen = true;
        let _ = ui.app.update_settings_quiet(settings);
    }
}

/// About: version, the media components' versions, copy and release notes.
unsafe fn show_about(ui: &MainUi) {
    let snapshot = ui.app.snapshot();
    let tools = snapshot
        .tools
        .iter()
        .map(|tool| {
            format!(
                "{} {}",
                tool.name,
                if tool.installed {
                    tool.version.as_str()
                } else {
                    "—"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let body = format!(
        "SSDownload {}\n{}\n\n{}\n\n{}",
        env!("CARGO_PKG_VERSION"),
        crate::i18n::ui(
            "Windows için dosya ve medya indirme yöneticisi.",
            "File and media download manager for Windows.",
        ),
        tools,
        crate::i18n::ui(
            "DRM korumalı içerikler desteklenmez.",
            "DRM-protected content is not supported.",
        ),
    );
    const COPY: i32 = 100;
    const NOTES: i32 = 101;
    let buttons = [
        (IDOK, crate::i18n::ui("Tamam", "OK")),
        (COPY, crate::i18n::ui("Kopyala", "Copy")),
        (NOTES, crate::i18n::ui("Sürüm notları", "Release notes")),
    ];
    match run_alert_buttons(
        ui.hwnd,
        &body,
        crate::i18n::ui("SSDownload hakkında", "About SSDownload"),
        MB_ICONINFORMATION,
        Some(&buttons),
    ) {
        Some(COPY) => {
            set_clipboard_text(&body);
        }
        Some(NOTES) => {
            ShellExecuteW(
                ui.hwnd,
                wide("open").as_ptr(),
                wide("https://ssdownload.isolmaz.com/changelog.html").as_ptr(),
                null(),
                null(),
                SW_SHOWNORMAL,
            );
        }
        _ => {}
    }
}

/// Largest URL list read by "Listeyi içe aktar".
const MAX_IMPORT_BYTES: u64 = 8 * 1024 * 1024;

/// The common open/save file dialog for plain-text URL lists.
unsafe fn text_file_dialog(owner: HWND, save: bool, default_name: &str) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let mut buffer = vec![0u16; 32768];
    for (index, unit) in default_name.encode_utf16().take(260).enumerate() {
        buffer[index] = unit;
    }
    let filter: Vec<u16> = crate::i18n::ui(
        "Metin dosyaları (*.txt)|*.txt|Tüm dosyalar (*.*)|*.*|",
        "Text files (*.txt)|*.txt|All files (*.*)|*.*|",
    )
    .encode_utf16()
    .map(|unit| if unit == u16::from(b'|') { 0 } else { unit })
    .chain(Some(0))
    .collect();
    let extension = wide("txt");
    let mut dialog: OPENFILENAMEW = zeroed();
    dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = owner;
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrDefExt = extension.as_ptr();
    dialog.Flags = OFN_EXPLORER
        | OFN_NOCHANGEDIR
        | OFN_PATHMUSTEXIST
        | if save {
            OFN_OVERWRITEPROMPT
        } else {
            OFN_FILEMUSTEXIST
        };
    let accepted = if save {
        GetSaveFileNameW(&mut dialog)
    } else {
        GetOpenFileNameW(&mut dialog)
    };
    if accepted == 0 {
        return None;
    }
    let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

/// Reads a text file of addresses (one per line; batch ranges allowed) and opens the
/// new-download dialog with them, so duplicates and options are confirmed as usual.
unsafe fn import_url_list(ui: &MainUi) {
    let Some(path) = text_file_dialog(ui.hwnd, false, "") else {
        return;
    };
    let read = std::fs::metadata(&path)
        .map_err(anyhow::Error::from)
        .and_then(|meta| {
            if meta.len() > MAX_IMPORT_BYTES {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Liste dosyası 8 MiB sınırını aşıyor.",
                        "The list file exceeds the 8 MiB limit.",
                    )
                );
            }
            Ok(std::fs::read(&path)?)
        });
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(error) => {
            message(
                ui.hwnd,
                &format!("{error:#}"),
                crate::i18n::ui("Listeyi içe aktar", "Import list"),
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines = text
        .lines()
        .map(|line| line.trim().trim_start_matches('\u{feff}'))
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("ftp://")
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        message(
            ui.hwnd,
            crate::i18n::ui(
                "Dosyada HTTP, HTTPS veya FTP adresi bulunamadı.",
                "The file contains no HTTP, HTTPS or FTP address.",
            ),
            crate::i18n::ui("Listeyi içe aktar", "Import list"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    show_add_dialog(ui.hwnd, ui.app.clone(), lines.join("\r\n"), false);
}

/// Writes the addresses of the selected downloads (all of them when nothing is
/// selected) to a UTF-8 text file, one per line.
unsafe fn export_url_list(ui: &MainUi) {
    let selected = selected_ids(ui);
    let urls = ui
        .snapshot
        .jobs
        .iter()
        .filter(|job| selected.is_empty() || selected.contains(&job.id))
        .map(|job| job.request.url.clone())
        .collect::<Vec<_>>();
    if urls.is_empty() {
        message(
            ui.hwnd,
            crate::i18n::ui(
                "Dışa aktarılacak indirme yok.",
                "There are no downloads to export.",
            ),
            crate::i18n::ui("Listeyi dışa aktar", "Export list"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    let Some(path) = text_file_dialog(ui.hwnd, true, "ssdownload-liste.txt") else {
        return;
    };
    let mut body = urls.join("\r\n");
    body.push_str("\r\n");
    if let Err(error) = std::fs::write(&path, body) {
        message(
            ui.hwnd,
            &format!("{error:#}"),
            crate::i18n::ui("Listeyi dışa aktar", "Export list"),
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    set_text(
        ui.status,
        &crate::i18n::ui_owned!(
            format!("{} adres dışa aktarıldı: {}", urls.len(), path.display()),
            format!("{} address(es) exported: {}", urls.len(), path.display())
        ),
    );
}

/// The taskbar button's progress (ITaskbarList3 through its raw vtable).
struct Taskbar {
    list: *mut c_void,
    state: u32,
    permille: u64,
}
const TBPF_NOPROGRESS: u32 = 0;
const TBPF_INDETERMINATE: u32 = 1;
const TBPF_NORMAL: u32 = 2;

impl Taskbar {
    unsafe fn create() -> Option<Self> {
        use windows_sys::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        const CLSID_TASKBAR_LIST: windows_sys::core::GUID =
            windows_sys::core::GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);
        const IID_TASKBAR_LIST3: windows_sys::core::GUID =
            windows_sys::core::GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eefaf);
        let mut list: *mut c_void = null_mut();
        if CoCreateInstance(
            &CLSID_TASKBAR_LIST,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_TASKBAR_LIST3,
            &mut list,
        ) != 0
            || list.is_null()
        {
            return None;
        }
        let taskbar = Self {
            list,
            state: u32::MAX,
            permille: u64::MAX,
        };
        let init: unsafe extern "system" fn(*mut c_void) -> i32 =
            std::mem::transmute(taskbar.method(3));
        if init(list) != 0 {
            return None;
        }
        Some(taskbar)
    }

    unsafe fn method(&self, index: usize) -> *const c_void {
        let vtable = *(self.list as *const *const *const c_void);
        *vtable.add(index)
    }

    unsafe fn show(&mut self, hwnd: HWND, state: u32, permille: u64) {
        if state == self.state && permille == self.permille {
            return;
        }
        let set_state: unsafe extern "system" fn(*mut c_void, HWND, u32) -> i32 =
            std::mem::transmute(self.method(10));
        set_state(self.list, hwnd, state);
        if state == TBPF_NORMAL {
            let set_value: unsafe extern "system" fn(*mut c_void, HWND, u64, u64) -> i32 =
                std::mem::transmute(self.method(9));
            set_value(self.list, hwnd, permille, 1000);
        }
        self.state = state;
        self.permille = permille;
    }
}
impl Drop for Taskbar {
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(self.method(2));
            release(self.list);
        }
    }
}

/// Aggregate progress of the running downloads on the taskbar button: normal with
/// the byte ratio when every running job knows its size, indeterminate otherwise,
/// and nothing while no download runs.
unsafe fn update_taskbar_progress(ui: &mut MainUi) {
    if ui.taskbar.is_none() {
        ui.taskbar = Taskbar::create();
    }
    let hwnd = ui.hwnd;
    let running = ui
        .snapshot
        .jobs
        .iter()
        .filter(|job| matches!(job.state, JobState::Downloading | JobState::Processing))
        .collect::<Vec<_>>();
    let (state, permille) = if running.is_empty() {
        (TBPF_NOPROGRESS, 0)
    } else if running
        .iter()
        .all(|job| job.total.is_some_and(|total| total > 0))
    {
        let total = running
            .iter()
            .map(|job| job.total.unwrap_or(0))
            .sum::<u64>();
        let done = running
            .iter()
            .map(|job| job.downloaded.min(job.total.unwrap_or(0)))
            .sum::<u64>();
        (
            TBPF_NORMAL,
            (done as u128 * 1000 / total.max(1) as u128) as u64,
        )
    } else {
        (TBPF_INDETERMINATE, 0)
    };
    if let Some(taskbar) = ui.taskbar.as_mut() {
        taskbar.show(hwnd, state, permille);
    }
}

/// How often one background notice may speak: the balloon follows the work, it
/// does not narrate every tick of it.
const NOTICE_INTERVAL: Duration = Duration::from_secs(3);
/// Bottom-right title of the dependency install and of an update.
const NOTICE_TITLE: &str = "SSDownload";
const UPDATE_NOTICE_TITLE: &str = "SSDownload güncellemesi";

/// What one bottom-right progress notice has already told the user. The
/// refresh tick runs several times a second, so the work behind it is compared
/// against this instead of being announced again on every repaint.
#[derive(Default)]
struct ProgressNotice {
    /// True while the work runs: the opening and the closing balloon both hang
    /// on this edge, and the closing one has to know it follows the opening.
    active: bool,
    /// Phase text of the last balloon.
    message: String,
    /// Whole percent of the last balloon, when the work reported a length.
    percent: Option<u32>,
    /// When the last balloon was sent.
    sent: Option<Instant>,
}

impl ProgressNotice {
    /// Whether a step is worth a balloon: the phase changed or the whole
    /// percent moved by five, and never closer than the notice interval.
    fn due(&self, message: &str, percent: Option<u32>, now: Instant) -> bool {
        let moved = message != self.message
            || match (percent, self.percent) {
                (Some(current), Some(last)) => current.abs_diff(last) >= 5,
                (None, None) => false,
                _ => true,
            };
        moved
            && self
                .sent
                .is_none_or(|sent| now.duration_since(sent) >= NOTICE_INTERVAL)
    }
    fn shown(&mut self, message: &str, percent: Option<u32>, now: Instant) {
        self.message.clear();
        self.message.push_str(message);
        self.percent = percent;
        self.sent = Some(now);
    }
    fn idle(&mut self) {
        self.active = false;
        self.message.clear();
        self.percent = None;
        self.sent = None;
    }
}

/// One balloon body: the phase text and, when the work can measure itself, the
/// whole percent.
fn notice_body(message: &str, percent: Option<u32>) -> String {
    match (percent, message.is_empty()) {
        (Some(percent), false) => format!("{message} · %{percent}"),
        (Some(percent), true) => format!("%{percent}"),
        (None, _) => message.to_string(),
    }
}

/// Whole percent of a tool download, `None` until its size is known.
fn tool_percent(progress: &ToolProgress) -> Option<u32> {
    let total = progress.total.filter(|total| *total > 0)?;
    Some((progress.downloaded.saturating_mul(100) / total).min(100) as u32)
}

/// The work the application reports on its own, polled from the same refresh
/// tick as the window: the dependency install it starts for the user, and a
/// running update. Both speak at the bottom right (tray balloon) and stop
/// without the user opening anything, and a running update also replaces the
/// status line for as long as it lasts.
unsafe fn refresh_notices(ui: &mut MainUi) {
    let now = Instant::now();
    let debug = ui.snapshot.settings.debug_mode;
    let progress = ui.snapshot.tool_progress.as_ref();
    let message = progress
        .map(|progress| progress.message.as_str())
        .unwrap_or("");
    let percent = progress.and_then(tool_percent);
    // A background refresh of an installed toolchain runs silently.
    let refreshing = ui.snapshot.tools.iter().all(|tool| tool.installed);
    if ui.snapshot.installing_tools && !refreshing {
        let pending = crate::i18n::ui(
            "Gerekli bağımlılıklar indiriliyor",
            "Downloading required dependencies",
        );
        if !ui.dependency_notice.active {
            ui.dependency_notice.active = true;
            ui.dependency_notice.shown(message, percent, now);
            notify(ui, NOTICE_TITLE, pending, false);
        } else if ui.dependency_notice.due(message, percent, now) {
            ui.dependency_notice.shown(message, percent, now);
            let body = notice_body(&display_text(debug, message), percent);
            notify(ui, NOTICE_TITLE, &body, false);
        }
        // The same state the balloon carries stays on the status line for as
        // long as the install lasts, so it can be read without waiting for a
        // balloon to appear.
        let body = notice_body(&display_text(debug, message), percent);
        let line = if body.is_empty() {
            pending.to_string()
        } else {
            format!("{pending} · {body}")
        };
        set_text(ui.status, &line);
    } else if ui.dependency_notice.active {
        ui.dependency_notice.idle();
        // The install is over, so the next balloon is its outcome: the media
        // support is there, or it is not and the reason is in the log. The
        // status line goes back to the queue counters either way.
        let installed = ui.snapshot.tools_error.is_none()
            && ui.snapshot.tools.iter().all(|tool| tool.installed);
        if installed {
            notify(
                ui,
                NOTICE_TITLE,
                crate::i18n::ui(
                    "Video ve ses desteği hazır.",
                    "Video and audio support is ready.",
                ),
                false,
            );
            update_status(ui);
        } else {
            // The retry has to be findable without the developer screen: the
            // balloon and the status line name it, and the same entry is enabled
            // in Araçlar while the components are missing or the last attempt
            // failed. The line stays until the next state change instead of
            // falling back to the counters, so the way back is not lost.
            notify(
                ui,
                NOTICE_TITLE,
                crate::i18n::ui(
                    "Video ve ses desteği indirilemedi. Araçlar > Medya desteğini kur ile yeniden deneyin.",
                    "Video and audio support could not be downloaded. Try again via Tools > Install media support.",
                ),
                true,
            );
            set_text(
                ui.status,
                crate::i18n::ui(
                    "Video ve ses desteği indirilemedi · Araçlar > Medya desteğini kur",
                    "Video and audio support could not be downloaded · Tools > Install media support",
                ),
            );
        }
    }

    // An update carries the status line while it lasts; the outcome dialog that
    // follows it is unchanged.
    match crate::update::progress() {
        Some(update) => {
            let percent = update.percent();
            let body = notice_body(&display_text(debug, &update.message), percent);
            let notice_title = crate::i18n::ui(UPDATE_NOTICE_TITLE, "SSDownload update");
            let line = if update.version.is_empty() {
                format!("{notice_title}: {body}")
            } else {
                format!("{notice_title} {}: {body}", update.version)
            };
            set_text(ui.status, &line);
            if !ui.update_notice.active {
                ui.update_notice.active = true;
                ui.update_notice.shown(&update.message, percent, now);
                notify(ui, notice_title, &body, false);
            } else if ui.update_notice.due(&update.message, percent, now) {
                ui.update_notice.shown(&update.message, percent, now);
                notify(ui, notice_title, &body, false);
            }
        }
        None => {
            if ui.update_notice.active {
                ui.update_notice.idle();
                update_status(ui);
            }
        }
    }
}

unsafe fn refresh(ui: &mut MainUi, force_rows: bool) {
    let snapshot = ui.app.snapshot();
    if snapshot.quit_requested {
        // `ssdownload-cli --quit`, and the engine's own quit action, land here.
        begin_exit(ui);
        request_exit(ui);
        return;
    }
    // A browser handoff brings its own picker: the extension no longer opens a
    // page of its own, so the window that hosts the picker is shown on the
    // originating monitor before the launch is taken. The launch sequence is
    // process-wide monotonic, so a handoff that arrived while another picker was
    // open is not skipped once this window can take it. A repeated click on the
    // same launch keeps its sequence number: the picker or the queued download's
    // own window raises itself for it instead of opening a second one.
    let launch = match snapshot.media_launch.as_ref() {
        Some(launch) if launch.seq != ui.last_launch_seq && !ui.exiting => {
            ui.last_launch_seq = launch.seq;
            Some(launch.clone())
        }
        _ => None,
    };
    if launch.is_some() {
        // A browser handoff opens only its picker. The main window keeps its state;
        // a minimized owner would hide the picker it owns, so it goes to the tray
        // instead of coming forward.
        if IsIconic(ui.hwnd) != 0 {
            ShowWindow(ui.hwnd, SW_HIDE);
        }
        ui.last_show_seq = snapshot.show_window_seq;
    } else if snapshot.show_window_seq != ui.last_show_seq {
        ui.last_show_seq = snapshot.show_window_seq;
        show_main(ui);
    }
    let seen_messages = ui.seen_messages;
    for item in snapshot.messages.iter().filter(|m| m.id > seen_messages) {
        ui.seen_messages = ui.seen_messages.max(item.id);
        let message = display_text(snapshot.settings.debug_mode, &item.text);
        set_text(ui.status, &message);
        if item.error {
            notify(
                ui,
                crate::i18n::ui("SSDownload hatası", "SSDownload error"),
                &message,
                true,
            );
        }
    }
    if snapshot.settings.dark_mode != ui.snapshot.settings.dark_mode {
        apply_dark_mode(ui.hwnd, snapshot.settings.dark_mode);
    }
    if snapshot.settings.clipboard_watch && !ui.snapshot.settings.clipboard_watch {
        handle_clipboard_change(ui);
    }
    apply_menu_mode(ui, &snapshot);
    update_tools_menu(ui, &snapshot);
    if snapshot.revision == ui.snapshot.revision && !force_rows {
        ui.snapshot = snapshot;
        refresh_notices(ui);
        if let Some(launch) = &launch {
            open_media_launch(ui, launch);
        }
        return;
    }
    let newly_completed: Vec<&Job> = snapshot
        .jobs
        .iter()
        .filter(|j| j.state == JobState::Completed && !ui.known_completed.contains(&j.id))
        .collect();
    let quiet = ui.quiet_until.is_some_and(|until| until > Instant::now());
    if snapshot.settings.completion_sound && !newly_completed.is_empty() && !quiet {
        windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(MB_ICONINFORMATION);
    }
    if let Some(job) = newly_completed.last() {
        ui.last_completed = Some(job.id.clone());
    }
    for job in &newly_completed {
        if job.open_when_done {
            let _ = ui.app.dispatch(Action::OpenFile { id: job.id.clone() });
        } else if snapshot.settings.completion_card && !quiet {
            open_card(ui.hwnd, ui.app.clone(), CardKind::Done(job.id.clone()));
        }
    }
    if snapshot.settings.notify_completion {
        for job in newly_completed {
            notify(
                ui,
                crate::i18n::ui("İndirme tamamlandı", "Download complete"),
                &job.name,
                false,
            );
        }
    }
    ui.known_completed = snapshot
        .jobs
        .iter()
        .filter(|j| j.state == JobState::Completed)
        .map(|j| j.id.clone())
        .collect();
    let changed = snapshot.revision != ui.snapshot.revision;
    let modes_changed = snapshot.settings.usage_modes != ui.snapshot.settings.usage_modes;
    ui.snapshot = snapshot;
    if modes_changed {
        update_profile_controls(ui);
    }
    if changed || force_rows {
        update_rows(ui, force_rows);
        update_details(ui);
    }
    update_taskbar_progress(ui);
    update_title_and_tray(ui);
    sync_hotkey(ui);
    update_filter_labels(ui);
    refresh_notices(ui);
    if let Some(launch) = &launch {
        open_media_launch(ui, launch);
    }
}

/// Status line: queue counters plus the current multi-selection. A pending engine
/// warning replaces it, matching the message stream that preceded this refactor.
unsafe fn update_status(ui: &MainUi) {
    let active = ui
        .snapshot
        .jobs
        .iter()
        .filter(|j| j.state.is_active())
        .count();
    let queued = ui
        .snapshot
        .jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Queued | JobState::Scheduled))
        .count();
    let failed = ui
        .snapshot
        .jobs
        .iter()
        .filter(|j| j.state == JobState::Failed)
        .count();
    let speed: u64 = ui.snapshot.jobs.iter().map(|j| j.speed).sum();
    let selection = selected_ids(ui).len();
    set_text(
        ui.status,
        &format!(
            "{} {} · {} {} · {} {} · {} {} · {}{}",
            ui.snapshot.jobs.len(),
            if ui.snapshot.jobs.len() == 1 {
                crate::i18n::ui("iş", "job")
            } else {
                crate::i18n::ui("iş", "jobs")
            },
            active,
            crate::i18n::ui("etkin", "active"),
            queued,
            crate::i18n::ui("bekliyor", "queued"),
            failed,
            if failed == 1 {
                crate::i18n::ui("hata", "error")
            } else {
                crate::i18n::ui("hata", "errors")
            },
            format_speed(speed),
            if selection > 1 {
                format!(" · {selection} {}", crate::i18n::ui("seçili", "selected"))
            } else {
                String::new()
            }
        ),
    );
    if let Some(warning) = &ui.snapshot.warning {
        set_text(
            ui.status,
            &display_text(ui.snapshot.settings.debug_mode, warning),
        );
    }
}

/// Every selected row, in list order. The list is multi-select, so batch
/// actions and the status line work from this set.
unsafe fn selected_ids(ui: &MainUi) -> Vec<String> {
    let mut ids = Vec::new();
    let mut index =
        SendMessageW(ui.list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32;
    while index >= 0 {
        if let Some(id) = ui.visible_ids.get(index as usize) {
            ids.push(id.clone());
        }
        index = SendMessageW(
            ui.list,
            LVM_GETNEXTITEM,
            index as usize,
            LVNI_SELECTED as isize,
        ) as i32;
    }
    ids
}

/// The first selected row; the details pane and single-job dialogs describe it.
unsafe fn selected_id(ui: &MainUi) -> Option<String> {
    selected_ids(ui).into_iter().next()
}
fn matches_filter(job: &Job, filter: Filter) -> bool {
    match filter {
        Filter::All => true,
        Filter::Active => matches!(
            job.state,
            JobState::Queued
                | JobState::Scheduled
                | JobState::Connecting
                | JobState::Downloading
                | JobState::Processing
                | JobState::Paused
        ),
        Filter::Completed => job.state == JobState::Completed,
        Filter::Errors => matches!(job.state, JobState::Failed | JobState::Cancelled),
        category => job_category(job) == Some(category),
    }
}
unsafe fn update_rows(ui: &mut MainUi, force: bool) {
    let query = search_key(text(ui.search).trim());
    let jobs: Vec<&Job> = ui
        .snapshot
        .jobs
        .iter()
        .filter(|j| {
            matches_filter(j, ui.filter_value)
                && (query.is_empty()
                    || search_key(&j.name).contains(&query)
                    || search_key(&j.request.url).contains(&query)
                    || search_key(&j.path.to_string_lossy()).contains(&query))
        })
        .collect();
    let ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
    let prior = selected_ids(ui);
    let rebuild = force || ids != ui.visible_ids;
    let mut changed = rebuild;
    if rebuild {
        ui.row_cache.clear();
        SendMessageW(ui.list, WM_SETREDRAW, 0, 0);
        SendMessageW(ui.list, LVM_DELETEALLITEMS, 0, 0);
        for (index, job) in jobs.iter().enumerate() {
            let mut name = wide(&job.name);
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: index as i32,
                iSubItem: 0,
                state: 0,
                stateMask: 0,
                pszText: name.as_mut_ptr(),
                cchTextMax: name.len() as i32,
                iImage: 0,
                lParam: 0,
                iIndent: 0,
                iGroupId: 0,
                cColumns: 0,
                puColumns: null_mut(),
                piColFmt: null_mut(),
                iGroup: 0,
            };
            if SendMessageW(ui.list, LVM_INSERTITEMW, 0, &item as *const _ as isize) < 0 {
                SendMessageW(ui.list, LVM_DELETEALLITEMS, 0, 0);
                ui.visible_ids.clear();
                SendMessageW(ui.list, WM_SETREDRAW, 1, 0);
                InvalidateRect(ui.list, null(), 0);
                return;
            }
        }
        ui.visible_ids = ids;
    }
    for (index, job) in jobs.iter().enumerate() {
        let cells = vec![
            job.name.clone(),
            state_label(job),
            format_progress(job),
            format_speed(job.speed),
            format_eta(job.eta),
            job.path.to_string_lossy().into_owned(),
        ];
        let old = ui.row_cache.get(&job.id);
        for (col, value) in cells.iter().enumerate() {
            if old.and_then(|values| values.get(col)) != Some(value) {
                set_list_cell(ui.list, index as i32, col as i32, value);
                changed = true;
            }
        }
        ui.row_cache.insert(job.id.clone(), cells);
    }
    if rebuild {
        for (order, id) in prior.iter().enumerate() {
            if let Some(pos) = ui.visible_ids.iter().position(|v| v == id) {
                let item = LVITEMW {
                    mask: LVIF_STATE,
                    iItem: pos as i32,
                    iSubItem: 0,
                    state: LVIS_SELECTED | if order == 0 { LVIS_FOCUSED } else { 0 },
                    stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                    pszText: null_mut(),
                    cchTextMax: 0,
                    iImage: 0,
                    lParam: 0,
                    iIndent: 0,
                    iGroupId: 0,
                    cColumns: 0,
                    puColumns: null_mut(),
                    piColFmt: null_mut(),
                    iGroup: 0,
                };
                SendMessageW(ui.list, LVM_SETITEMSTATE, pos, &item as *const _ as isize);
            }
        }
    }
    SendMessageW(ui.list, WM_SETREDRAW, 1, 0);
    if changed {
        InvalidateRect(ui.list, null(), 0);
    }
}
unsafe fn set_list_cell(list: HWND, row: i32, col: i32, value: &str) {
    let mut w = wide(value);
    let mut item: LVITEMW = zeroed();
    item.iSubItem = col;
    item.pszText = w.as_mut_ptr();
    SendMessageW(
        list,
        LVM_SETITEMTEXTW,
        row as usize,
        &item as *const _ as isize,
    );
}

fn search_key(value: &str) -> String {
    value.replace('İ', "i").replace('I', "ı").to_lowercase()
}
fn source_refresh_available(job: &Job) -> bool {
    let has_page = job
        .request
        .source_identity
        .as_ref()
        .is_some_and(|identity| !identity.page_url.is_empty())
        || job
            .request
            .page_url
            .as_deref()
            .is_some_and(|url| !url.is_empty());
    job.remove_requested.is_none()
        && has_page
        && matches!(job.state, JobState::AwaitingSource | JobState::Failed)
}
fn can_pause_or_resume(job: &Job) -> bool {
    job.remove_requested.is_none()
        && job.state != JobState::Completed
        && !(job.browser_transfer_authorized
            && matches!(
                job.state,
                JobState::Paused | JobState::Failed | JobState::Cancelled
            ))
}
fn short_error(job: &Job, value: &str) -> &'static str {
    let lower = value.to_ascii_lowercase();
    if source_refresh_available(job)
        && (job.state == JobState::AwaitingSource
            || lower.contains("kaynak")
            || lower.contains("expired")
            || lower.contains("403"))
    {
        crate::i18n::ui(
            "Kaynak adresi yenilenmeli; satır menüsünden yenilemeyi başlatın.",
            "The source address must be refreshed; start a refresh from the row menu.",
        )
    } else if lower.contains("permission") || lower.contains("izin") || lower.contains("consent") {
        crate::i18n::ui(
            "Site izni gerekiyor; tarayıcı uzantısında bu siteye izin verin.",
            "Site permission is required; allow this site in the browser extension.",
        )
    } else if lower.contains("unsupported") || lower.contains("desteklen") || lower.contains("drm")
    {
        crate::i18n::ui(
            "Bu kaynak veya biçim desteklenmiyor; başka bir seçim yapın.",
            "This source or format is not supported; make a different choice.",
        )
    } else if matches!(job.state, JobState::Failed | JobState::Cancelled) {
        crate::i18n::ui(
            "İş başarısız oldu; satır menüsünden yeniden deneyin veya Hata ayrıntısı'nı açın.",
            "The job failed; retry from the row menu or open the error details.",
        )
    } else {
        crate::i18n::ui("Hata ayrıntısını açın.", "Open the error details.")
    }
}

fn display_url(value: &str) -> String {
    if let Ok(mut url) = url::Url::parse(value) {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    } else {
        crate::i18n::ui("[adres gizlendi]", "[address hidden]").into()
    }
}
unsafe fn update_details(ui: &MainUi) {
    let ids = selected_ids(ui);
    let jobs: Vec<&Job> = ids
        .iter()
        .filter_map(|id| ui.snapshot.jobs.iter().find(|job| &job.id == id))
        .collect();
    for (id, enabled) in [
        (
            ID_PAUSE_RESUME,
            jobs.iter().any(|job| can_pause_or_resume(job)),
        ),
        (
            ID_REMOVE,
            jobs.iter().any(|job| job.remove_requested.is_none()),
        ),
        (
            ID_OPEN_FOLDER,
            jobs.iter().any(|job| job.remove_requested.is_none()),
        ),
        (
            ID_OPEN_FILE,
            jobs.iter()
                .any(|job| job.state == JobState::Completed && job.remove_requested.is_none()),
        ),
        (ID_JOB_LOG, !jobs.is_empty()),
        (
            ID_SOURCE_REFRESH,
            jobs.iter().any(|job| source_refresh_available(job)),
        ),
    ] {
        // Job log and source refresh only exist in the menu.
        let control = GetDlgItem(ui.hwnd, id);
        if !control.is_null() {
            EnableWindow(control, enabled as BOOL);
        }
        EnableMenuItem(
            GetMenu(ui.hwnd),
            id as u32,
            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
        );
    }

    if jobs.is_empty() {
        set_text(
            ui.details,
            crate::i18n::ui(
                "Bir indirme işi seçin. URL eklemek için Ctrl+N tuşlarına basın veya bağlantıyı pencereye sürükleyin.",
                "Select a download job. Press Ctrl+N to add a URL or drag a link onto the window.",
            ),
        );
        update_status(ui);
        return;
    }
    if jobs.len() > 1 {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for job in &jobs {
            let label = state_label(job);
            match counts.iter_mut().find(|(name, _)| *name == label) {
                Some((_, count)) => *count += 1,
                None => counts.push((label, 1)),
            }
        }
        let breakdown = counts
            .iter()
            .map(|(label, count)| format!("{label}: {count}"))
            .collect::<Vec<_>>()
            .join(" · ");
        let speed: u64 = jobs.iter().map(|job| job.speed).sum();
        set_text(
            ui.details,
            &format!(
                "{} {}\r\n{}: {}\r\n{}: {}\r\n\r\n{}",
                jobs.len(),
                crate::i18n::ui("iş seçili.", "jobs selected."),
                crate::i18n::ui("Durumlar", "States"),
                breakdown,
                crate::i18n::ui("Toplam hız", "Total speed"),
                format_speed(speed),
                crate::i18n::ui(
                    "Kaldır, Duraklat / Sürdür ve Aç eylemleri seçilen tüm işlere uygulanır.",
                    "Remove, Pause / Resume and Open actions apply to all selected jobs.",
                ),
            ),
        );
        update_status(ui);
        return;
    }
    let job = jobs[0];
    let kind = match job.request.kind {
        DownloadKind::Auto => crate::i18n::ui("Otomatik", "Automatic"),
        DownloadKind::File => crate::i18n::ui("Dosya", "File"),
        DownloadKind::Video => crate::i18n::ui("Video", "Video"),
        DownloadKind::Audio => crate::i18n::ui("Ses", "Audio"),
    };
    let scheduled = job
        .request
        .start_at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".into());
    let body = format!("{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}\r\n{}: {}{}",
        crate::i18n::ui("Ad", "Name"), job.name,
        crate::i18n::ui("Durum", "State"), state_label(job),
        crate::i18n::ui("Tür", "Type"), kind,
        crate::i18n::ui("İlerleme", "Progress"), format_progress(job),
        crate::i18n::ui("Hız", "Speed"), format_speed(job.speed),
        crate::i18n::ui("Kalan", "Remaining"), format_eta(job.eta),
        crate::i18n::ui("Deneme", "Attempts"), job.attempts,
        crate::i18n::ui("Zamanlama", "Schedule"), scheduled,
        crate::i18n::ui("Konum", "Location"), job.path.display(),
        crate::i18n::ui("URL", "URL"), display_url(&job.request.url),
        job.error.as_ref().map(|error| format!("\r\n{}: {}", crate::i18n::ui("Hata", "Error"), display_text(ui.snapshot.settings.debug_mode, short_error(job, error)))).unwrap_or_default());
    set_text(ui.details, &body);
    update_status(ui);
}

/// Ctrl+A. Text controls keep their own select-all behavior, so the shortcut
/// never hijacks typing in the search box or the details pane.
unsafe fn select_all_jobs(ui: &mut MainUi) {
    let focus = GetFocus();
    if focus == ui.search || focus == ui.details {
        SendMessageW(focus, EM_SETSEL, 0, -1);
        return;
    }
    if ui.visible_ids.is_empty() {
        return;
    }
    let mut item: LVITEMW = zeroed();
    item.stateMask = LVIS_SELECTED;
    item.state = LVIS_SELECTED;
    SendMessageW(
        ui.list,
        LVM_SETITEMSTATE,
        usize::MAX,
        &item as *const _ as isize,
    );
    update_details(ui);
}

unsafe fn pause_resume_selected(ui: &MainUi) {
    let ids = selected_ids(ui);
    let snapshot = ui.app.snapshot();
    for id in ids {
        let Some(job) = snapshot.jobs.iter().find(|job| job.id == id) else {
            continue;
        };
        if !can_pause_or_resume(job) {
            continue;
        }
        match job.state {
            JobState::Paused | JobState::Failed | JobState::Cancelled => {
                dispatch(ui, Action::Resume { id })
            }
            JobState::Completed => {}
            _ => dispatch(ui, Action::Pause { id }),
        }
    }
}
unsafe fn remove_selected(ui: &MainUi) {
    let snapshot = ui.app.snapshot();
    let targets: Vec<String> = selected_ids(ui)
        .into_iter()
        .filter(|id| {
            snapshot
                .jobs
                .iter()
                .find(|job| &job.id == id)
                .is_some_and(|job| job.remove_requested.is_none())
        })
        .collect();
    let Some(first) = targets.first() else {
        return;
    };
    let (body, title) = if targets.len() == 1 {
        let name = snapshot
            .jobs
            .iter()
            .find(|job| &job.id == first)
            .map(|job| job.name.as_str())
            .unwrap_or_default();
        (
            crate::i18n::ui_owned!(format!("\"{name}\" kuyruktan kaldırılsın mı?\n\nEvet: indirilen/parça dosyasını da sil\nHayır: dosyayı diskte bırak\nİptal: işlem yapma"), format!("Remove \"{name}\" from the queue?\n\nYes: also delete the downloaded/partial file\nNo: keep the file on disk\nCancel: do nothing")),
            crate::i18n::ui("İndirmeyi kaldır", "Remove download"),
        )
    } else {
        (
            crate::i18n::ui_owned!(format!(
                "{} indirme kuyruktan kaldırılsın mı?\n\nEvet: indirilen/parça dosyalarını da sil\nHayır: dosyaları diskte bırak\nİptal: işlem yapma",
                targets.len()
            ), format!(
                "Remove {} downloads from the queue?\n\nYes: also delete the downloaded/partial files\nNo: keep the files on disk\nCancel: do nothing",
                targets.len()
            )),
            crate::i18n::ui("İndirmeleri kaldır", "Remove downloads"),
        )
    };
    let answer = message(
        ui.hwnd,
        &body,
        title,
        MB_YESNOCANCEL | MB_ICONWARNING | MB_DEFBUTTON3,
    );
    if answer != IDYES && answer != IDNO {
        return;
    }
    let delete_file = answer == IDYES;
    for id in targets {
        dispatch(ui, Action::Remove { id, delete_file });
    }
}
unsafe fn open_selected_folder(ui: &MainUi) {
    let ids = selected_ids(ui);
    if ids.is_empty() {
        dispatch(ui, Action::OpenFolder { id: None });
        return;
    }
    let snapshot = ui.app.snapshot();
    for id in ids {
        if snapshot
            .jobs
            .iter()
            .find(|job| job.id == id)
            .is_some_and(|job| job.remove_requested.is_some())
        {
            continue;
        }
        dispatch(ui, Action::OpenFolder { id: Some(id) });
    }
}
unsafe fn open_selected_file(ui: &MainUi) {
    let snapshot = ui.app.snapshot();
    for id in selected_ids(ui) {
        if !snapshot
            .jobs
            .iter()
            .find(|job| job.id == id)
            .is_some_and(|job| job.state == JobState::Completed && job.remove_requested.is_none())
        {
            continue;
        }
        dispatch(ui, Action::OpenFile { id });
    }
}
/// Starts the exit: quit the engine and mark the window as going away. Every
/// route that ends the session goes through here.
unsafe fn begin_exit(ui: &mut MainUi) {
    if ui.exiting {
        return;
    }
    ui.exiting = true;
    save_placement(ui);
    let _ = ui.app.dispatch(Action::Quit);
    ui.app.shutdown();
}

/// Asks the dialogs to close and lets the window be destroyed from a fresh
/// message. The window is never destroyed here: the current handler can be a
/// frame of the window about to be freed, with a modal loop - and the handler
/// that opened that dialog - below it on the stack (the wizard writes its
/// snapshot back into MainUi right after its loop, the Add dialog re-enables the
/// window). Once the dialogs are gone those frames have returned.
unsafe fn request_exit(ui: &MainUi) {
    if close_top_dialog(ui.hwnd) {
        PostMessageW(ui.hwnd, WM_EXIT_AFTER_DIALOGS, 1, 0);
    } else {
        PostMessageW(ui.hwnd, WM_EXIT_AFTER_DIALOGS, EXIT_CLOSE_ROUNDS + 1, 0);
    }
}

unsafe fn exit_app(ui: &mut MainUi) {
    begin_exit(ui);
    request_exit(ui);
}
/// Finds one live dialog owned by `owner`, topmost first. The app's modal
/// dialogs are top-level windows owned by the main window, and the owner chain
/// is the reliable handle on them: GetLastActivePopup answers with the owner
/// itself once the owner is enabled again (a nested dialog re-enables it), which
/// hid a dialog that was still open, so the owner was destroyed under a live
/// dialog and the frame that opened it kept writing into the freed window.
struct PopupSearch {
    owner: HWND,
    found: Vec<HWND>,
}

unsafe extern "system" fn popup_search(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = &mut *(lparam as *mut PopupSearch);
    if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER) == search.owner {
        search.found.push(hwnd);
    }
    1
}

/// Every visible dialog owned by `owner`, topmost first (EnumWindows walks the
/// z-order). The app's dialogs are top-level windows owned by the main window,
/// and the owner chain is the reliable handle on them: GetLastActivePopup answers
/// with the owner itself once the owner is enabled again (a nested dialog
/// re-enables it), which hid a dialog that was still open.
unsafe fn owned_dialogs(owner: HWND) -> Vec<HWND> {
    let mut search = PopupSearch {
        owner,
        found: Vec::new(),
    };
    EnumWindows(Some(popup_search), &mut search as *mut _ as LPARAM);
    search.found
}

/// Closes the main window's topmost dialog through WM_CLOSE, so its own teardown
/// runs; true when a dialog was asked to close.
unsafe fn close_top_dialog(owner: HWND) -> bool {
    let dialogs = owned_dialogs(owner);
    match dialogs.first() {
        Some(&top) => {
            PostMessageW(top, WM_CLOSE, 0, 0);
            true
        }
        None => false,
    }
}

/// Closes every remaining dialog, still through its own WM_CLOSE, and destroys
/// the ones that refuse it: a window that never answers must not hold the exit.
unsafe fn force_owned_dialogs(owner: HWND) {
    for dialog in owned_dialogs(owner) {
        SendMessageW(dialog, WM_CLOSE, 0, 0);
        if IsWindow(dialog) != 0 {
            DestroyWindow(dialog);
        }
    }
}
unsafe fn show_main(ui: &MainUi) {
    ShowWindow(ui.hwnd, SW_RESTORE);
    keep_window_on_screen(ui.hwnd);
    ShowWindow(ui.hwnd, SW_SHOW);
    let popup = GetLastActivePopup(ui.hwnd);
    let target = if !popup.is_null() && IsWindow(popup) != 0 && IsWindowVisible(popup) != 0 {
        popup
    } else {
        ui.hwnd
    };
    if IsIconic(target) != 0 {
        ShowWindow(target, SW_RESTORE);
    }
    keep_window_on_screen(target);
    activate_window(target);
    if ui.wizard_pending && target == ui.hwnd && !wizard_auto_open_suppressed() {
        PostMessageW(ui.hwnd, WM_SHOW_WIZARD, 0, 0);
    }
}

unsafe fn begin_source_refresh(ui: &MainUi) {
    let snapshot = ui.app.snapshot();
    let targets: Vec<String> = selected_ids(ui)
        .into_iter()
        .filter(|id| {
            snapshot
                .jobs
                .iter()
                .find(|job| &job.id == id)
                .is_some_and(source_refresh_available)
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let total = targets.len();
    let mut opened = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for id in targets {
        let name = snapshot
            .jobs
            .iter()
            .find(|job| job.id == id)
            .map(|job| job.name.clone())
            .unwrap_or_default();
        match ui.app.dispatch_result(Action::BeginSourceRefresh { id }) {
            Ok(result) => {
                let Some(ticket) = result.source_refresh else {
                    failures.push(format!(
                        "{name}: {}",
                        crate::i18n::ui(
                            "kaynak yenileme bağlantısı alınamadı.",
                            "the source refresh link could not be obtained.",
                        )
                    ));
                    continue;
                };
                if !url::Url::parse(&ticket.page_url)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                {
                    failures.push(format!(
                        "{name}: {}",
                        crate::i18n::ui(
                            "işin kayıtlı sayfası güvenli bir HTTP/HTTPS adresi değil.",
                            "the job's saved page is not a safe HTTP/HTTPS address.",
                        )
                    ));
                    continue;
                }
                crate::logging::record(
                    crate::logging::Event::info("shell.open_page")
                        .host(crate::logging::host_of(&ticket.page_url).unwrap_or_default())
                        .detail(format!(
                            "kaynak yenileme sayfası açılıyor path={}",
                            crate::logging::sanitize(&ticket.page_url)
                        )),
                );
                let opened_page = ShellExecuteW(
                    ui.hwnd,
                    wide("open").as_ptr(),
                    wide(&ticket.page_url).as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                );
                if opened_page as isize <= 32 {
                    failures.push(format!(
                        "{name}: {} {}",
                        crate::i18n::ui(
                            "ilgili sayfa açılamadı. Bu sayfayı açın:",
                            "the related page could not be opened. Open this page:",
                        ),
                        display_url(&ticket.page_url)
                    ));
                } else {
                    opened += 1;
                }
            }
            Err(error) => failures.push(format!(
                "{name}: {}: {error:#}",
                crate::i18n::ui(
                    "kaynak yenileme başlatılamadı",
                    "the source refresh could not be started",
                )
            )),
        }
    }
    let refresh_title = crate::i18n::ui("Kaynak adresini yenile", "Refresh source address");
    if !failures.is_empty() {
        message(
            ui.hwnd,
            &failures.join("\n"),
            refresh_title,
            MB_OK | MB_ICONERROR,
        );
    } else if total == 1 {
        message(
            ui.hwnd,
            crate::i18n::ui(
                "İlgili sayfa açıldı. Tarayıcıdaki SSDownload düğmesi bu bekleyen işe güvenli biçimde bağlanacaktır.",
                "The related page was opened. The SSDownload button in the browser will safely link it to this pending job.",
            ),
            refresh_title,
            MB_OK | MB_ICONINFORMATION,
        );
    } else if opened > 0 {
        message(
            ui.hwnd,
            &format!(
                "{} {}",
                opened,
                crate::i18n::ui(
                    "kaynak yenileme sayfası açıldı. Tarayıcıdaki SSDownload düğmesi bekleyen işlere güvenli biçimde bağlanacaktır.",
                    "source refresh pages were opened. The SSDownload button in the browser will safely link them to the pending jobs.",
                )
            ),
            refresh_title,
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Menu entry behind "İş günlüğü...": the first row of the current
/// selection, matching the details pane.
unsafe fn show_selected_job_log(ui: &MainUi) {
    let Some(id) = selected_id(ui) else {
        return;
    };
    if let Some(job) = ui.snapshot.jobs.iter().find(|job| job.id == id) {
        show_job_log(ui.hwnd, ui.app.clone(), &job.id, &job.name);
    }
}

unsafe fn show_job_menu(ui: &mut MainUi, position: LPARAM) {
    let mut point = POINT {
        x: loword(position as usize) as i16 as i32,
        y: hiword(position as usize) as i16 as i32,
    };
    if position != -1 {
        let mut hit: LVHITTESTINFO = zeroed();
        hit.pt = point;
        ScreenToClient(ui.list, &mut hit.pt);
        let index = SendMessageW(ui.list, LVM_HITTEST, 0, &mut hit as *mut _ as LPARAM);
        if index < 0 {
            return;
        }
        let mut item: LVITEMW = zeroed();
        item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        // A right-click inside the current multi-selection keeps it, so batch
        // actions stay reachable; any other row becomes the new selection.
        if SendMessageW(
            ui.list,
            LVM_GETITEMSTATE,
            index as usize,
            LVIS_SELECTED as isize,
        ) == 0
        {
            SendMessageW(
                ui.list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &item as *const _ as LPARAM,
            );
        }
        item.state = LVIS_SELECTED | LVIS_FOCUSED;
        SendMessageW(
            ui.list,
            LVM_SETITEMSTATE,
            index as usize,
            &item as *const _ as LPARAM,
        );
    } else {
        let index = SendMessageW(
            ui.list,
            LVM_GETNEXTITEM,
            usize::MAX,
            LVNI_SELECTED as LPARAM,
        );
        if index < 0 {
            return;
        }
        let mut rect: RECT = zeroed();
        rect.left = LVIR_BOUNDS as i32;
        SendMessageW(
            ui.list,
            LVM_GETITEMRECT,
            index as usize,
            &mut rect as *mut _ as LPARAM,
        );
        point = POINT {
            x: rect.left,
            y: rect.bottom,
        };
        ClientToScreen(ui.list, &mut point);
    }
    // Actions follow current engine state, independently of the last painted row.
    let snapshot = ui.app.snapshot();
    let jobs: Vec<&Job> = selected_ids(ui)
        .iter()
        .filter_map(|id| snapshot.jobs.iter().find(|job| &job.id == id))
        .collect();
    let (Some(first), count) = (jobs.first().copied(), jobs.len()) else {
        return;
    };
    let menu = CreatePopupMenu();
    let mut error = None;
    if count == 1 {
        let job = first;
        if job.state == JobState::Completed && job.remove_requested.is_none() {
            append(menu, ID_OPEN_FILE, crate::i18n::ui("Aç", "Open"));
        }
        if job.remove_requested.is_none() {
            append(
                menu,
                ID_OPEN_FOLDER,
                crate::i18n::ui("Klasörde göster", "Show in folder"),
            );
        }
        if source_refresh_available(job) {
            append(
                menu,
                ID_SOURCE_REFRESH,
                crate::i18n::ui("Kaynak adresini yenile...", "Refresh source address..."),
            );
        }
        if can_pause_or_resume(job) {
            append(
                menu,
                ID_PAUSE_RESUME,
                match job.state {
                    JobState::Paused => crate::i18n::ui("Sürdür", "Resume"),
                    JobState::Failed | JobState::Cancelled => {
                        crate::i18n::ui("Yeniden dene", "Retry")
                    }
                    JobState::AwaitingSource => crate::i18n::ui(
                        "Yenilemeyi iptal et ve duraklat",
                        "Cancel refresh and pause",
                    ),
                    _ => crate::i18n::ui("Duraklat", "Pause"),
                },
            );
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        append(
            menu,
            ID_COPY_URL,
            crate::i18n::ui("Adresi kopyala", "Copy address"),
        );
        append(
            menu,
            ID_REDOWNLOAD,
            crate::i18n::ui(
                "Aynı klasöre yeniden indir",
                "Download again to the same folder",
            ),
        );
        if job.state == JobState::Completed && job.remove_requested.is_none() {
            append(
                menu,
                ID_RENAME,
                crate::i18n::ui("Yeniden adlandır...", "Rename..."),
            );
        }
        if !job.state.is_terminal() && job.remove_requested.is_none() {
            AppendMenuW(
                menu,
                MF_STRING | if job.open_when_done { MF_CHECKED } else { 0 },
                ID_OPEN_WHEN_DONE as usize,
                wide(crate::i18n::ui("Tamamlanınca aç", "Open when done")).as_ptr(),
            );
            if matches!(
                job.state,
                JobState::Queued | JobState::Scheduled | JobState::Paused
            ) {
                append(
                    menu,
                    ID_START_NOW,
                    crate::i18n::ui("Hemen başlat", "Start now"),
                );
            }
        }
        append(
            menu,
            ID_MOVE_TOP,
            crate::i18n::ui("En üste taşı\tAlt+Home", "Move to top\tAlt+Home"),
        );
        append(
            menu,
            ID_MOVE_BOTTOM,
            crate::i18n::ui("En alta taşı\tAlt+End", "Move to bottom\tAlt+End"),
        );
        if !ui.ui_mode.is_simple() && ui.snapshot.settings.queues.len() > 1 {
            let queues = CreatePopupMenu();
            for (index, queue) in ui.snapshot.settings.queues.iter().enumerate().take(64) {
                let current = job.request.queue_id.as_deref() == Some(queue.id.as_str());
                AppendMenuW(
                    queues,
                    MF_STRING | if current { MF_CHECKED } else { 0 },
                    (ID_QUEUE_BASE + index as i32) as usize,
                    wide(&queue.name).as_ptr(),
                );
            }
            AppendMenuW(
                menu,
                MF_POPUP,
                queues as usize,
                wide(crate::i18n::ui("Kuyruğa taşı", "Move to queue")).as_ptr(),
            );
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        if job.remove_requested.is_none() {
            append(menu, ID_REMOVE, crate::i18n::ui("Kaldır...", "Remove..."));
        }
        if !ui.ui_mode.is_simple() {
            append(
                menu,
                ID_JOB_LOG,
                crate::i18n::ui("İş günlüğünü göster...", "Show job log..."),
            );
        }
        error = job.error.clone();
        if error.is_some() {
            append(
                menu,
                ID_JOB_ERROR,
                crate::i18n::ui("Hata ayrıntısı", "Error details"),
            );
        }
    } else {
        let openable = jobs
            .iter()
            .filter(|job| job.state == JobState::Completed && job.remove_requested.is_none())
            .count();
        if openable > 0 {
            append(
                menu,
                ID_OPEN_FILE,
                &format!("{} ({openable})", crate::i18n::ui("Aç", "Open")),
            );
        }
        let foldered = jobs
            .iter()
            .filter(|job| job.remove_requested.is_none())
            .count();
        if foldered > 0 {
            append(
                menu,
                ID_OPEN_FOLDER,
                &format!(
                    "{} ({foldered})",
                    crate::i18n::ui("Klasörde göster", "Show in folder")
                ),
            );
        }
        let renewable = jobs
            .iter()
            .filter(|job| source_refresh_available(job))
            .count();
        if renewable > 0 {
            append(
                menu,
                ID_SOURCE_REFRESH,
                &format!(
                    "{} ({renewable})",
                    crate::i18n::ui("Kaynak adresini yenile...", "Refresh source address...")
                ),
            );
        }
        let pausable: Vec<&&Job> = jobs.iter().filter(|job| can_pause_or_resume(job)).collect();
        if !pausable.is_empty() {
            let resumable = pausable
                .iter()
                .filter(|job| {
                    matches!(
                        job.state,
                        JobState::Paused | JobState::Failed | JobState::Cancelled
                    )
                })
                .count();
            let label = if resumable == pausable.len() {
                format!(
                    "{} ({})",
                    crate::i18n::ui("Sürdür", "Resume"),
                    pausable.len()
                )
            } else if resumable == 0 {
                format!(
                    "{} ({})",
                    crate::i18n::ui("Duraklat", "Pause"),
                    pausable.len()
                )
            } else {
                format!(
                    "{} ({})",
                    crate::i18n::ui("Duraklat / Sürdür", "Pause / Resume"),
                    pausable.len()
                )
            };
            append(menu, ID_PAUSE_RESUME, &label);
        }
        append(
            menu,
            ID_COPY_URL,
            &format!(
                "{} ({count})",
                crate::i18n::ui("Adresleri kopyala", "Copy addresses")
            ),
        );
        append(
            menu,
            ID_START_NOW,
            &format!("{} ({count})", crate::i18n::ui("Hemen başlat", "Start now")),
        );
        append(
            menu,
            ID_MOVE_TOP,
            crate::i18n::ui("En üste taşı\tAlt+Home", "Move to top\tAlt+Home"),
        );
        append(
            menu,
            ID_MOVE_BOTTOM,
            crate::i18n::ui("En alta taşı\tAlt+End", "Move to bottom\tAlt+End"),
        );
        if foldered > 0 {
            append(
                menu,
                ID_REMOVE,
                &format!("{} ({foldered})", crate::i18n::ui("Kaldır...", "Remove...")),
            );
        }
    }
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        point.x,
        point.y,
        0,
        ui.hwnd,
        null(),
    ) as i32;
    DestroyMenu(menu);
    PostMessageW(ui.hwnd, WM_NULL, 0, 0);
    if command == ID_JOB_ERROR {
        if let Some(error) = error {
            message(
                ui.hwnd,
                &error,
                crate::i18n::ui("Hata ayrıntısı", "Error details"),
                MB_OK | MB_ICONERROR,
            );
        }
    } else if command == ID_JOB_LOG {
        show_job_log(ui.hwnd, ui.app.clone(), &first.id, &first.name);
    } else if command != 0 {
        main_command(ui, command, 0);
    }
}

unsafe fn add_tray(ui: &mut MainUi) {
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = ui.hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = ui.icon;
    copy_wide(&mut data.szTip, "SSDownload");
    if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
        ui.tray_added = true;
    }
}
unsafe fn remove_tray(ui: &mut MainUi) {
    if ui.tray_added {
        let mut data: NOTIFYICONDATAW = zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = ui.hwnd;
        data.uID = TRAY_ID;
        Shell_NotifyIconW(NIM_DELETE, &data);
        ui.tray_added = false;
    }
}
fn copy_wide<const N: usize>(dest: &mut [u16; N], src: &str) {
    let value: Vec<u16> = src.encode_utf16().take(N - 1).collect();
    dest[..value.len()].copy_from_slice(&value);
    dest[value.len()] = 0;
}
unsafe fn notify(ui: &MainUi, title: &str, body: &str, error: bool) {
    if !ui.tray_added || (!error && ui.quiet_until.is_some_and(|until| until > Instant::now())) {
        return;
    }
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = ui.hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_INFO;
    copy_wide(&mut data.szInfoTitle, title);
    copy_wide(&mut data.szInfo, body);
    data.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO };
    Shell_NotifyIconW(NIM_MODIFY, &data);
}
/// The tray's right-click menu: the everyday actions without opening the window.
unsafe fn tray_menu(ui: &MainUi) -> HMENU {
    let settings = &ui.snapshot.settings;
    let menu = CreatePopupMenu();
    append(
        menu,
        ID_TRAY_SHOW,
        crate::i18n::ui("SSDownload'u aç", "Open SSDownload"),
    );
    append(
        menu,
        ID_MINI_PANEL,
        crate::i18n::ui("Mini panel", "Mini panel"),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    append(
        menu,
        ID_TRAY_ADD,
        crate::i18n::ui("Yeni indirme...", "New download..."),
    );
    append(
        menu,
        ID_TRAY_CLIPBOARD,
        crate::i18n::ui("Panodan ekle", "Add from clipboard"),
    );
    append(
        menu,
        ID_BATCH,
        crate::i18n::ui("Toplu indirme...", "Batch download..."),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    append(
        menu,
        ID_TRAY_PAUSE,
        crate::i18n::ui("Tümünü duraklat", "Pause all"),
    );
    append(
        menu,
        ID_TRAY_RESUME,
        crate::i18n::ui("Tümünü sürdür", "Resume all"),
    );
    let speed = CreatePopupMenu();
    for (id, label, kib) in [
        (
            ID_TRAY_SPEED_UNLIMITED,
            crate::i18n::ui("Sınırsız", "Unlimited"),
            0u64,
        ),
        (ID_TRAY_SPEED_1M, crate::i18n::ui("1 MB/sn", "1 MB/s"), 1024),
        (
            ID_TRAY_SPEED_512K,
            crate::i18n::ui("512 KB/sn", "512 KB/s"),
            512,
        ),
    ] {
        let flags = MF_STRING
            | if settings.speed_limit_kib == kib {
                MF_CHECKED
            } else {
                0
            };
        AppendMenuW(speed, flags, id as usize, wide(label).as_ptr());
    }
    let custom = ![0u64, 512, 1024].contains(&settings.speed_limit_kib);
    AppendMenuW(
        speed,
        MF_STRING | if custom { MF_CHECKED } else { 0 },
        ID_TRAY_SPEED_CUSTOM as usize,
        wide(crate::i18n::ui("Özel...", "Custom...")).as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_POPUP,
        speed as usize,
        wide(crate::i18n::ui("Hız sınırı", "Speed limit")).as_ptr(),
    );
    let shutdown = settings
        .queues
        .iter()
        .find(|queue| queue.id == crate::model::DEFAULT_QUEUE_ID)
        .is_some_and(|queue| matches!(queue.completion, CompletionAction::ShutdownComputer { .. }));
    AppendMenuW(
        menu,
        MF_STRING | if shutdown { MF_CHECKED } else { 0 },
        ID_TRAY_SHUTDOWN as usize,
        wide(crate::i18n::ui(
            "Bitince bilgisayarı kapat",
            "Shut down when done",
        ))
        .as_ptr(),
    );
    let quiet = ui.quiet_until.is_some_and(|until| until > Instant::now());
    AppendMenuW(
        menu,
        MF_STRING | if quiet { MF_CHECKED } else { 0 },
        ID_TRAY_QUIET as usize,
        wide(crate::i18n::ui(
            "1 saat bildirim gösterme",
            "Mute notifications for 1 hour",
        ))
        .as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    append(
        menu,
        ID_SETTINGS,
        crate::i18n::ui("Ayarlar...", "Settings..."),
    );
    match crate::update::available_version() {
        Some(version) => append(
            menu,
            ID_INSTALL_UPDATE,
            &crate::i18n::ui_owned!(
                format!("Güncellemeyi kur ({version})..."),
                format!("Install update ({version})...")
            ),
        ),
        None => append(
            menu,
            ID_UPDATE_CHECK,
            crate::i18n::ui("Güncellemeleri denetle...", "Check for updates..."),
        ),
    }
    append(
        menu,
        ID_TRAY_FOLDER,
        crate::i18n::ui("İndirme klasörünü aç", "Open download folder"),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    append(menu, ID_EXIT, crate::i18n::ui("Çıkış", "Exit"));
    menu
}

unsafe fn handle_tray(ui: &mut MainUi, event: u32) {
    match event {
        NIN_BALLOONUSERCLICK_ => {
            // The balloon of a completed download opens its folder.
            if let Some(id) = ui.last_completed.clone() {
                let _ = ui.app.dispatch(Action::OpenFolder { id: Some(id) });
            } else {
                show_main(ui);
            }
        }
        WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT => show_main(ui),
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            let menu = tray_menu(ui);
            let mut p: POINT = zeroed();
            GetCursorPos(&mut p);
            SetForegroundWindow(ui.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                p.x,
                p.y,
                0,
                ui.hwnd,
                null(),
            ) as i32;
            DestroyMenu(menu);
            PostMessageW(ui.hwnd, WM_NULL, 0, 0);
            if cmd != 0 {
                main_command(ui, cmd, 0);
            }
        }
        _ => {}
    }
}

unsafe fn clipboard_text() -> String {
    if IsClipboardFormatAvailable(CF_UNICODETEXT_) == 0 || OpenClipboard(null_mut()) == 0 {
        return String::new();
    }
    let handle = GetClipboardData(CF_UNICODETEXT_);
    if handle.is_null() {
        CloseClipboard();
        return String::new();
    }
    let ptr = GlobalLock(handle) as *const u16;
    if ptr.is_null() {
        CloseClipboard();
        return String::new();
    }
    let capacity = (GlobalSize(handle) / 2).min(1_048_576);
    let mut len = 0usize;
    while len < capacity && *ptr.add(len) != 0 {
        len += 1;
    }
    let value = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    GlobalUnlock(handle);
    CloseClipboard();
    value
}
unsafe fn clipboard_urls() -> Vec<String> {
    extract_urls(&clipboard_text())
}
unsafe fn handle_clipboard_change(ui: &mut MainUi) {
    if !ui.snapshot.settings.clipboard_watch {
        return;
    }
    let value = clipboard_text();
    if value.is_empty() || value == ui.clipboard_sequence_text {
        return;
    }
    ui.clipboard_sequence_text = value.clone();
    let urls = extract_urls(&value);
    if !urls.is_empty() {
        notify(
            ui,
            crate::i18n::ui("Bağlantı bulundu", "Link found"),
            crate::i18n::ui(
                "Yeni indirme için Ctrl+N tuşlarına basın.",
                "Press Ctrl+N for a new download.",
            ),
            false,
        );
    }
}
unsafe fn handle_drop(ui: &MainUi, drop: HDROP) {
    use std::io::Read;
    const INPUT_LIMIT: usize = 1024 * 1024;
    let mut remaining = INPUT_LIMIT;
    let mut too_large = false;
    let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
    let mut collected = Vec::new();
    for i in 0..count {
        let n = DragQueryFileW(drop, i, null_mut(), 0);
        let mut buf = vec![0u16; n as usize + 1];
        DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
        let path = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
        if path.extension().and_then(|v| v.to_str()).is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "url" | "txt" | "m3u" | "m3u8"
            )
        }) {
            let mut content = String::new();
            if std::fs::File::open(&path)
                .and_then(|file| file.take(remaining as u64 + 1).read_to_string(&mut content))
                .is_ok()
            {
                if content.len() > remaining {
                    too_large = true;
                    break;
                }
                remaining -= content.len();
                collected.extend(extract_urls(&content));
            }
        } else if let Some(s) = path.to_str() {
            collected.extend(extract_urls(s));
        }
    }
    DragFinish(drop);
    if too_large {
        message(
            ui.hwnd,
            crate::i18n::ui(
                "Bırakılan metin dosyaları toplam 1 MiB sınırını aşıyor.",
                "The dropped text files exceed the combined 1 MiB limit.",
            ),
            crate::i18n::ui("Sürükle ve bırak", "Drag and drop"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    if !collected.is_empty() {
        show_add_dialog(ui.hwnd, ui.app.clone(), collected.join("\r\n"), false);
    } else {
        message(
            ui.hwnd,
            crate::i18n::ui(
                "Bırakılan öğelerde HTTP/HTTPS bağlantısı bulunamadı.",
                "No HTTP/HTTPS link was found in the dropped items.",
            ),
            crate::i18n::ui("Sürükle ve bırak", "Drag and drop"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}
/// Chrome colours of the dark scheme. Every dark surface, frame and separator
/// in the application comes from this list so the scheme stays one palette.
const DARK_BACKGROUND: u32 = 0x202020;
const DARK_SURFACE: u32 = 0x333333;
const DARK_BORDER: u32 = 0x4d4d4d;
const DARK_TEXT: u32 = 0xeeeeee;
const DARK_MUTED: u32 = 0xa0a0a0;
const DARK_SELECTION: u32 = 0x6f4f2f; // COLORREF: renders RGB(47,79,111)
/// The same fill, dimmed, for the rest of a multi selection (COLORREF is BGR:
/// this renders RGB(31,42,51)).
const DARK_SELECTION_DIM: u32 = 0x332a1f;
const DARK_ACCENT: u32 = 0xa86e2f; // COLORREF: renders RGB(47,110,168)
/// Tab strip of the settings dialog.
const S_TABS: i32 = 2100;
const S_DIR: i32 = 2101;
const S_BROWSE: i32 = 2102;
const S_MAX_ACTIVE: i32 = 2103;
const S_CONNECTIONS: i32 = 2104;
const S_PER_HOST: i32 = 2105;
const S_SPEED: i32 = 2106;
const S_RETRY: i32 = 2107;
const S_MEDIA_FRAGMENTS: i32 = 2108;
const S_CLIPBOARD: i32 = 2110;
const S_TRAY: i32 = 2111;
const S_STARTUP: i32 = 2112;
const S_NOTIFY: i32 = 2113;
const S_SCHEDULE: i32 = 2114;
const S_START: i32 = 2115;
const S_END: i32 = 2116;
const S_DARK: i32 = 2117;
const S_OK: i32 = 2118;
const S_UPDATE_CHECK: i32 = 2119;
const S_LOG_LEVEL: i32 = 2120;
const S_LOG_HOSTS: i32 = 2121;
/// Surface depth of the settings dialog itself and the browser-entry toggle,
/// both on the Genel page.
const S_UI_MODE: i32 = 2122;
const S_SITE_ENTRIES: i32 = 2123;
/// Interface language selector on the Genel page.
const S_UI_LANGUAGE: i32 = 2124;
const S_PROXY_MODE: i32 = 2125;
const S_PROXY_URL: i32 = 2126;
const S_PROXY_USER: i32 = 2127;
const S_PROXY_PASS: i32 = 2128;
const S_SPEED_SCHEDULE: i32 = 2129;
const S_SPEED_START: i32 = 2130;
const S_SPEED_END: i32 = 2131;
const S_SPEED_KIB: i32 = 2132;
const S_KEEP_AWAKE: i32 = 2133;
const S_SOUND: i32 = 2134;
const S_TAKEOVER: i32 = 2135;
const S_CARD: i32 = 2136;
const S_METERED: i32 = 2137;
const S_HOTKEY: i32 = 2138;
const S_DOUBLE_CLICK: i32 = 2139;
const S_HISTORY: i32 = 2140;
const SP_EDIT: i32 = 2701;
const RN_EDIT: i32 = 2703;
const RN_OK: i32 = 2704;
const P_MINI: i32 = 2451;
const P_PIN: i32 = 2452;
const SP_OK: i32 = 2702;
const L_LIST: i32 = 2711;
const L_HOST: i32 = 2712;
const L_USER: i32 = 2713;
const L_PASS: i32 = 2714;
const L_ALLOW_HTTP: i32 = 2715;
const L_ADD: i32 = 2716;
const L_REMOVE: i32 = 2717;
const L_SAVE: i32 = 2718;
const T_STATUS: i32 = 2201;
const T_PROGRESS: i32 = 2202;
const T_INSTALL: i32 = 2203;
const T_UPDATE: i32 = 2204;
const T_BROWSER: i32 = 2205;
const M_SUMMARY: i32 = 2301;
const M_KIND: i32 = 2302;
const M_FORMAT: i32 = 2303;
const M_AUDIO: i32 = 2304;
const M_NAME: i32 = 2311;
const M_PLAYLIST: i32 = 2306;
const M_QUEUE: i32 = 2307;
const M_RENEW_JOB: i32 = 2308;
const M_RENEW_RESTART: i32 = 2309;
const M_RENEW: i32 = 2310;
/// "Hata bildir" on the picker: hidden until the analysis itself failed, so the
/// normal flow never shows a button that cannot do anything.
const M_REPORT: i32 = 2312;
const M_MP3: i32 = 2313;
const M_REMEMBER: i32 = 2314;
const M_PLAYLIST_ITEMS: i32 = 2315;
const M_THUMBNAIL: i32 = 2316;
/// A background thumbnail load hands its bitmap to the picker (lparam = HBITMAP).
pub(super) const WM_THUMBNAIL: u32 = WM_APP + 40;

/// The queued handoff's own window (`DialogKind::Progress`). These ids are the
/// stable contract automation drives: status text, its progress bar, the
/// pause/resume/retry button, cancel, the two output actions and close.
const P_STATUS: i32 = 2440;
const P_PROGRESS: i32 = 2441;
const P_ACTION: i32 = 2442;
const P_CANCEL: i32 = 2443;
const P_OPEN_FILE: i32 = 2444;
const P_OPEN_FOLDER: i32 = 2445;
const P_CLOSE: i32 = 2446;
/// Playlist rows, the summary line and the close hint: ids beyond the driven
/// contract, so a handoff that returned several jobs is worked one by one
/// instead of being watched through an arbitrary first one.
const P_SUMMARY: i32 = 2447;
const P_LIST: i32 = 2448;
const P_HINT: i32 = 2449;
/// "Hata bildir" on the job view: hidden while the watched job is fine.
const P_REPORT: i32 = 2450;

struct AddFields {
    urls: HWND,
    dir: HWND,
    filename: HWND,
    kind: HWND,
    kind_values: Vec<DownloadKind>,
    connections: HWND,
    checksum: HWND,
    schedule: HWND,
    initial: String,
    analyze_first: bool,
    advanced: bool,
}
struct WizardFields {
    modes: UsageModes,
    simple: bool,
    video: HWND,
    file: HWND,
    audio: HWND,
    simple_card: HWND,
    advanced_card: HWND,
    start: HWND,
}
struct SettingsFields {
    value: Settings,
    dir: HWND,
    max_active: HWND,
    connections: HWND,
    media_fragments: HWND,
    per_host: HWND,
    speed: HWND,
    retry: HWND,
    clipboard: HWND,
    tray: HWND,
    startup: HWND,
    notify: HWND,
    schedule: HWND,
    start: HWND,
    end: HWND,
    dark: HWND,
    mode_video: HWND,
    mode_file: HWND,
    mode_audio: HWND,
    browser_transfer: HWND,
    update_check: HWND,
    log_level: HWND,
    log_hosts: HWND,
    ui_mode: HWND,
    site_entries: HWND,
    ui_language: HWND,
    proxy_mode: HWND,
    proxy_url: HWND,
    proxy_user: HWND,
    proxy_pass: HWND,
    speed_schedule: HWND,
    speed_start: HWND,
    speed_end: HWND,
    speed_kib: HWND,
    keep_awake: HWND,
    sound: HWND,
    takeover: HWND,
    completion_card: HWND,
    metered: HWND,
    hotkey: HWND,
    double_click: HWND,
    history: HWND,
}
struct ToolsFields {
    status: HWND,
    progress: HWND,
    install: HWND,
    update: HWND,
    last_render: String,
}
struct QueueFields {
    list: HWND,
    ids: Vec<String>,
    name: HWND,
    concurrency: HWND,
    completion: HWND,
    countdown: HWND,
    enabled: HWND,
    windows: HWND,
    quota: HWND,
    program: HWND,
    arguments: HWND,
    delay: HWND,
    job_id: Option<String>,
}
struct RuleFields {
    list: HWND,
    ids: Vec<String>,
    host: HWND,
    dir: HWND,
    subdomains: HWND,
    file: HWND,
    video: HWND,
    audio: HWND,
    priority: HWND,
}
struct SpeedFields {
    ids: Vec<String>,
    initial: u64,
    edit: HWND,
}
struct RenameFields {
    id: String,
    initial: String,
    edit: HWND,
}
struct LoginFields {
    logins: Vec<crate::model::SiteLogin>,
    list: HWND,
    host: HWND,
    user: HWND,
    pass: HWND,
    allow_http: HWND,
}
struct CrawlerFields {
    url: HWND,
    depth: HWND,
    pages: HWND,
    candidates: HWND,
    list: HWND,
    result: Option<SiteCrawlResult>,
    pending: Option<std::sync::mpsc::Receiver<std::result::Result<SiteCrawlResult, String>>>,
}
struct SyncFields {
    list: HWND,
    ids: Vec<String>,
    url: HWND,
    dir: HWND,
    interval: HWND,
    overwrite: HWND,
    enabled: HWND,
}
struct MediaFields {
    base: AddRequest,
    /// Session preference: the user explicitly chose to embed no subtitle track.
    subtitle_none: bool,
    /// Whether the browser handoff carried this site's session cookies: `None`
    /// for a handoff-less analysis, where no grant was asked for.
    session_consent: Option<bool>,
    summary: HWND,
    /// Editable file name the selection will be saved under.
    name: HWND,
    kind: HWND,
    kind_values: Vec<(DownloadKind, Option<String>)>,
    format: HWND,
    audio: HWND,
    audio_tracks: HWND,
    subtitles: HWND,
    external_url: HWND,
    external_language: HWND,
    external_label: HWND,
    external_kind: HWND,
    external_default: HWND,
    external_list: HWND,
    full_verification: HWND,
    playlist: HWND,
    /// Pending source-retry jobs this handoff can rebind, in combo order.
    renew_job: HWND,
    renew_restart: HWND,
    renew_ids: Vec<String>,
    video_formats: Vec<Option<MediaFormat>>,
    audio_values: Vec<AudioSelection>,
    subtitle_values: Vec<SubtitleTrack>,
    external_values: Vec<ExternalSubtitle>,
    loaded_title: String,
    /// Whether `populate_media` has filled these controls from this dialog's
    /// analysis; the queue button is offered only for the selection on screen.
    populated: bool,
    /// The engine's analysis generation this dialog was created for; a newer
    /// handoff or analysis invalidates the selection that is on screen.
    inspect_generation: u64,
    advanced: bool,
    /// Whether the report button is on screen: set from the snapshot, because
    /// only a failed analysis has anything to report.
    report_shown: bool,
    /// Failure of a report dispatch the user has not read past yet; it stays on
    /// the error line until the picker leaves the failed state instead of being
    /// overwritten by the inspection error on the next tick.
    report_error: Option<String>,
    /// Site the handoff came from, for the remembered choice.
    host: String,
    /// "Bir daha sorma" for this site: queue as soon as the analysis loads.
    auto_queue: bool,
    remember: HWND,
    playlist_items: HWND,
    thumbnail: HWND,
    thumbnail_requested: bool,
}
/// The job view one queued handoff turns into. It watches the exact ids
/// `Action::Add` returned, in the engine's own order: an unrelated job that
/// lands meanwhile can never take their place, and a job that is gone is
/// reported as gone instead of being shown as running.
struct ProgressFields {
    ids: Vec<String>,
    /// Index into `ids` whose detail the status area and the controls show.
    /// Filled from the playlist rows, never from a list position of the main
    /// window.
    selected: usize,
    /// Jobs whose completion was already brought to the front: a re-render of
    /// the same completed state must not pull the window forward again.
    raised: HashSet<String>,
    /// Last rendered detail text, so the read-only status is rewritten only
    /// when the engine state it shows actually changed.
    rendered: String,
    /// Last rendered playlist rows, for the same reason.
    rendered_rows: String,
    /// Last label of the pause/resume/retry button.
    action_label: String,
    summary: HWND,
    list: HWND,
    status: HWND,
    progress: HWND,
    action: HWND,
    cancel: HWND,
    open_file: HWND,
    open_folder: HWND,
    close: HWND,
    hint: HWND,
    report: HWND,
}
struct EventFields {
    list: HWND,
    rendered: Vec<String>,
}
/// One download's own timeline, as written to `logs/jobs/job-<id>.jsonl`.
struct JobLogFields {
    job: String,
    list: HWND,
    rendered: Vec<String>,
}
enum DialogKind {
    Wizard(WizardFields),
    Add(AddFields),
    Settings(Box<SettingsFields>),
    Tools(ToolsFields),
    Queues(QueueFields),
    Rules(RuleFields),
    Crawler(CrawlerFields),
    Synchronization(SyncFields),
    Events(EventFields),
    JobLog(JobLogFields),
    Media(Box<MediaFields>),
    Speed(SpeedFields),
    Rename(RenameFields),
    Logins(LoginFields),
    /// What a picker becomes once its selection was queued: the same window,
    /// the same box, driven by the job ids the engine returned.
    Progress(Box<ProgressFields>),
}
struct DialogUi {
    app: App,
    hwnd: HWND,
    font: HFONT,
    error: HWND,
    dpi: u32,
    scroll_y: std::cell::Cell<i32>,
    /// Content height behind the scrollable area, zero when everything fits.
    scroll_range: std::cell::Cell<i32>,
    /// Selected settings tab; other dialogs leave it at zero.
    tab: std::cell::Cell<usize>,
    /// Set by the timer when a newer handoff replaced this picker, so the
    /// create failure it causes is reported as a cancellation, not an error.
    superseded: std::cell::Cell<bool>,
    /// Nothing disables the owner for this window and no modal loop drives it:
    /// the application keeps working while it is open, so the loop that pumps
    /// its messages has to hand the window's own keyboard handling to the
    /// dialog manager (`dialog_message`).
    modeless: bool,
    /// The window owns this state box and frees it on the last message that can
    /// still reach it. Only a modeless creator sets this, and only once the
    /// window was created: until then the creating frame owns the box, because
    /// a dialog destroyed during its own WM_CREATE never reaches the callback
    /// that would free it.
    owned: bool,
    callbacks: usize,
    destroyed: bool,
    /// Browser handoff this window belongs to. While it lives the handoff stays
    /// current, so a repeated click raises this window instead of starting a
    /// second analysis, and a second window for one request cannot be opened.
    launch: Option<u64>,
    /// Raise counter of that handoff this window has already honoured.
    raise_seq: u64,
    kind: DialogKind,
    /// The logical size the layout was designed for; the window may not be
    /// shrunk below it.
    design: (i32, i32),
}

// Nested alert/file-dialog loops can destroy a modeless owner while its outer
// command handler still runs. Keep its state until that last callback returns.
struct DialogCallback(*mut DialogUi);
impl Drop for DialogCallback {
    fn drop(&mut self) {
        unsafe {
            let state = &mut *self.0;
            state.callbacks -= 1;
            if state.callbacks == 0 && state.destroyed && state.owned {
                drop(Box::from_raw(self.0));
            }
        }
    }
}

/// Everything a dialog window is created with. The modal loop and the modeless
/// picker share it, so a window is framed, sized and placed the same way in
/// both.
struct DialogRequest<'a> {
    title: &'a str,
    width: i32,
    height: i32,
    kind: DialogKind,
    origin: Option<[i32; 4]>,
    /// `(launch sequence, raise counter)` for a window that belongs to a browser
    /// handoff; `None` for the dialogs the application opens for itself.
    launch: Option<(u64, u64)>,
    /// No modal loop: nothing disables the owner and the window frees its own
    /// box when it is destroyed.
    modeless: bool,
}

/// Moves a window, keeping its size, to the centre of `origin`'s monitor, then
/// creates the dialog state it will run on. The caller owns the returned box
/// until it hands the window the ownership flag.
///
/// `None` means no window exists: either the creation failed, or a newer handoff
/// replaced this dialog inside its own WM_CREATE - which the handoff already
/// accounted for, so nothing is reported and nothing is retried. The box is
/// freed on both paths, which is why an `owned` window cannot be freed twice by
/// the destroy that gets here.
unsafe fn create_dialog(
    owner: HWND,
    app: App,
    request: DialogRequest<'_>,
) -> Option<(*mut DialogUi, HWND)> {
    let DialogRequest {
        title,
        width,
        height,
        kind,
        origin,
        launch,
        modeless,
    } = request;
    // CreateWindowExW sends WM_CREATE from inside the call, before it returns
    // and before the failure it reports can be inspected. A later handoff can
    // replace this picker inside that message, so clear the thread error here
    // and let the create handler mark the replacement; the failure branch then
    // tells a cancelled picker apart from a real create error.
    SetLastError(0);
    let (launch_seq, raise_seq) = match launch {
        Some((seq, raised)) => (Some(seq), raised),
        None => (None, 0),
    };
    let raw = Box::into_raw(Box::new(DialogUi {
        app,
        hwnd: null_mut(),
        font: null_mut(),
        error: null_mut(),
        dpi: 96,
        scroll_y: std::cell::Cell::new(0),
        scroll_range: std::cell::Cell::new(0),
        tab: std::cell::Cell::new(0),
        superseded: std::cell::Cell::new(false),
        modeless,
        owned: false,
        callbacks: 0,
        destroyed: false,
        launch: launch_seq,
        raise_seq,
        kind,
        design: (width, height),
    }));
    // A window whose owner is hidden (the main window in the tray) gets its own
    // taskbar button, so a browser handoff's picker stands on its own.
    let own_button = if owner.is_null() || IsWindowVisible(owner) == 0 {
        WS_EX_APPWINDOW
    } else {
        0
    };
    let hwnd = CreateWindowExW(
        WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT | own_button,
        wide(CLASS_DIALOG).as_ptr(),
        wide(title).as_ptr(),
        WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_CLIPCHILDREN | WS_VSCROLL,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        scale(width, GetDpiForWindow(owner).max(96)),
        scale(height, GetDpiForWindow(owner).max(96)),
        owner,
        null_mut(),
        GetModuleHandleW(null()),
        raw.cast(),
    );
    if hwnd.is_null() {
        // A picker that a newer handoff replaced destroys itself during
        // WM_CREATE, so CreateWindowExW reports a failure for a cancellation
        // the handoff already accounted for. The newer launch opens its own
        // picker on the next refresh tick: nothing to report, nothing to retry.
        if (*raw).superseded.get() {
            drop(Box::from_raw(raw));
            return None;
        }
        let error = GetLastError();
        drop(Box::from_raw(raw));
        message(
            owner,
            &crate::i18n::ui_owned!(
                format!("İletişim penceresi açılamadı (Win32 {error})."),
                format!("Couldn't open the contact window (Win32 {error}).")
            ),
            "SSDownload",
            MB_OK | MB_ICONERROR,
        );
        return None;
    }
    center_window(hwnd, owner);
    if let Some(origin) = origin {
        place_window_on(hwnd, Some(origin));
    }
    if (*raw).destroyed {
        drop(Box::from_raw(raw));
        return None;
    }
    (*raw).owned = modeless;
    Some((raw, hwnd))
}

/// Opens a window that stays modeless: the owner is never disabled, the window
/// drives itself from its own timer, and it lives exactly until it is
/// destroyed. Used by the media picker, whose window has to outlive a long
/// download without holding the application.
unsafe fn show_modeless(owner: HWND, app: App, request: DialogRequest<'_>) -> HWND {
    match create_dialog(owner, app, request) {
        Some((_raw, hwnd)) => {
            ShowWindow(hwnd, SW_SHOW);
            activate_window(hwnd);
            hwnd
        }
        None => null_mut(),
    }
}

/// Brings one of this process's own windows to the front, once.
///
/// `SetForegroundWindow` is refused to a process that does not own the
/// foreground and was not granted the right to take it. A browser handoff
/// transfers that right through the bridge for the picker; a download that
/// finishes while the user is working in another application has no such grant.
/// For that case the thread input of the window that owns the foreground is
/// attached for exactly the calls that need shared input state, and detached in
/// every path including the failure one. Nothing here is global or synthetic:
/// no keystrokes are injected, no foreground-lock setting is changed, the
/// window is never made topmost, and the attach never outlives this call.
unsafe fn activate_window(hwnd: HWND) -> bool {
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return false;
    }
    if IsIconic(hwnd) != 0 {
        ShowWindow(hwnd, SW_RESTORE);
    }
    if IsWindowVisible(hwnd) == 0 {
        ShowWindow(hwnd, SW_SHOW);
    }
    // The front of its own process's z-order; the foreground raise below is the
    // one that matters, and this one still lands when that is refused.
    SetWindowPos(
        hwnd,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    if SetForegroundWindow(hwnd) != 0 && GetForegroundWindow() == hwnd {
        return true;
    }
    let foreground = GetForegroundWindow();
    if foreground.is_null() || foreground == hwnd {
        return GetForegroundWindow() == hwnd;
    }
    let mut process = 0u32;
    let foreground_thread = GetWindowThreadProcessId(foreground, &mut process);
    let current_thread = GetWindowThreadProcessId(hwnd, null_mut());
    if foreground_thread == 0 || current_thread == 0 || foreground_thread == current_thread {
        return GetForegroundWindow() == hwnd;
    }
    if AttachThreadInput(foreground_thread, current_thread, 1) == 0 {
        return GetForegroundWindow() == hwnd;
    }
    BringWindowToTop(hwnd);
    let raised = SetForegroundWindow(hwnd);
    // Detached in the same call that attached it, whatever the two calls above
    // reported: shared input state is never left behind for the next window.
    AttachThreadInput(foreground_thread, current_thread, 0);
    raised != 0 && GetForegroundWindow() == hwnd
}

/// The live dialog state behind a window, when that window is one of ours. The
/// class is checked first: the user data slot only ever holds a `DialogUi` for
/// `CLASS_DIALOG` windows, and `WM_NCDESTROY` clears it before the box is
/// freed, so a window that is going away answers with nothing.
unsafe fn dialog_state(hwnd: HWND) -> *mut DialogUi {
    if hwnd.is_null() || !window_class(hwnd).eq_ignore_ascii_case(CLASS_DIALOG) {
        return null_mut();
    }
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogUi
}

/// Runs one message through the dialog manager when it belongs to a dialog of
/// ours: keyboard navigation, the default button and Escape all come from there.
/// A modal dialog pumps its own loop through this; a modeless dialog has no loop
/// of its own, so whichever loop is pumping at the time - the main one, or a
/// modal dialog's nested one - has to run the same call for it. Messages for
/// anything else, the main window included, are left to their own window.
unsafe fn dialog_message(dialog: HWND, msg: &MSG) -> bool {
    if msg.hwnd.is_null() {
        return IsDialogMessageW(dialog, msg) != 0;
    }
    let root = GetAncestor(msg.hwnd, GA_ROOT);
    if root == dialog {
        return IsDialogMessageW(dialog, msg) != 0;
    }
    let state = dialog_state(root);
    if state.is_null() || !(*state).modeless {
        return false;
    }
    IsDialogMessageW(root, msg) != 0
}

/// True when a window of this process other than `owner` holds the foreground:
/// a dialog that opened another window - the picker that took over the queued
/// download - must not pull the foreground back to its owner when it closes.
unsafe fn another_window_in_front(owner: HWND) -> bool {
    let foreground = GetForegroundWindow();
    if foreground.is_null() || foreground == owner {
        return false;
    }
    let mut expected = 0u32;
    let mut current = 0u32;
    GetWindowThreadProcessId(owner, &mut expected);
    GetWindowThreadProcessId(foreground, &mut current);
    expected != 0 && current == expected
}

/// Opens a modal dialog. Dialogs that must be placed on a monitor other than
/// the owner's call `run_modal_at` with an explicit origin.
unsafe fn run_modal(owner: HWND, app: App, title: &str, width: i32, height: i32, kind: DialogKind) {
    run_modal_at(owner, app, title, width, height, kind, None)
}

unsafe fn run_modal_at(
    owner: HWND,
    app: App,
    title: &str,
    width: i32,
    height: i32,
    kind: DialogKind,
    origin: Option<[i32; 4]>,
) {
    let Some((raw, hwnd)) = create_dialog(
        owner,
        app,
        DialogRequest {
            title,
            width,
            height,
            kind,
            origin,
            launch: None,
            modeless: false,
        },
    ) else {
        return;
    };
    EnableWindow(owner, 0);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    let mut msg: MSG = zeroed();
    let mut repost_quit = false;
    while IsWindow(hwnd) != 0 {
        let got = GetMessageW(&mut msg, null_mut(), 0, 0);
        if got <= 0 {
            repost_quit = got == 0;
            break;
        }
        if IsWindow(hwnd) == 0 {
            // GetMessage delivers sent messages while it waits, so the dialog can
            // be gone before this loop sees the message that woke it. The box is
            // still ours here, but there is nothing left to drive.
            break;
        }
        // The dialog manager hands the arrow keys to the focused control, so
        // the scroll keys are taken here, on the wheel's own clamp.
        let dialog = &*raw;
        if (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN)
            && dialog.scroll_range.get() > 0
        {
            let delta = scroll_delta_for_key(msg.wParam as u16, dialog.dpi);
            if delta != 0 {
                dialog
                    .scroll_y
                    .set(dialog.scroll_y.get().saturating_add(delta).max(0));
                layout_dialog(dialog);
                continue;
            }
        }
        if !dialog_message(hwnd, &msg) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    EnableWindow(owner, 1);
    if IsWindow(owner) != 0 && !another_window_in_front(owner) {
        SetForegroundWindow(owner);
    }
    if IsWindow(hwnd) != 0 {
        // A quit request ends the loop with the dialog alive. It is destroyed
        // with its user data still in place, so WM_NCDESTROY runs the timer and
        // font cleanup and clears the slot itself; the box lives until the drop
        // below, so those frames see live state.
        DestroyWindow(hwnd);
    }
    // Single owner: the box outlives every handler and every nested frame that
    // can still touch it, and dies here.
    drop(Box::from_raw(raw));
    if repost_quit {
        PostQuitMessage(0);
    }
}
/// Work area of the monitor that contains `origin` (a full monitor rect).
unsafe fn monitor_work_area(origin: [i32; 4]) -> Option<RECT> {
    let rect = RECT {
        left: origin[0],
        top: origin[1],
        right: origin[2],
        bottom: origin[3],
    };
    let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return None;
    }
    Some(info.rcWork)
}
/// Monitor rect of the window that owns the foreground, captured when a browser
/// handoff arrives: the picker opens where the browser is.
pub fn foreground_monitor() -> Option<[i32; 4]> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        Some([
            info.rcMonitor.left,
            info.rcMonitor.top,
            info.rcMonitor.right,
            info.rcMonitor.bottom,
        ])
    }
}
/// Moves a window, keeping its size, to the centre of `origin`'s monitor. Used
/// before the first show so no frame ever lands on another monitor.
unsafe fn place_window_on(hwnd: HWND, origin: Option<[i32; 4]>) {
    let Some(origin) = origin else {
        return;
    };
    let Some(area) = monitor_work_area(origin) else {
        return;
    };
    let mut wr: RECT = zeroed();
    if GetWindowRect(hwnd, &mut wr) == 0 {
        return;
    }
    let width = wr.right - wr.left;
    let height = wr.bottom - wr.top;
    let x = ((area.left + area.right) / 2 - width / 2)
        .clamp(area.left, (area.right - width).max(area.left));
    let y = ((area.top + area.bottom) / 2 - height / 2)
        .clamp(area.top, (area.bottom - height).max(area.top));
    SetWindowPos(
        hwnd,
        null_mut(),
        x,
        y,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}
unsafe fn center_window(hwnd: HWND, owner: HWND) {
    let mut wr: RECT = zeroed();
    if GetWindowRect(hwnd, &mut wr) == 0 {
        return;
    }
    // An ownerless alert has no rectangle to centre on, and centring on an
    // empty one puts it in the desktop's top-left corner. The application's
    // main window is the surface the user is looking at, so it anchors those
    // alerts as well as their monitor.
    let anchor = if !owner.is_null() && IsWindow(owner) != 0 {
        owner
    } else {
        FindWindowW(wide(CLASS_MAIN).as_ptr(), null())
    };
    let mut or: RECT = zeroed();
    let anchored = !anchor.is_null() && GetWindowRect(anchor, &mut or) != 0;
    let monitor = MonitorFromWindow(
        if anchored { anchor } else { hwnd },
        MONITOR_DEFAULTTONEAREST,
    );
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(monitor, &mut info);
    let area = info.rcWork;
    let width = (wr.right - wr.left).min(area.right - area.left);
    let height = (wr.bottom - wr.top).min(area.bottom - area.top);
    let (centre_x, centre_y) = if anchored {
        ((or.left + or.right) / 2, (or.top + or.bottom) / 2)
    } else {
        ((area.left + area.right) / 2, (area.top + area.bottom) / 2)
    };
    let x = (centre_x - width / 2).clamp(area.left, (area.right - width).max(area.left));
    let y = (centre_y - height / 2).clamp(area.top, (area.bottom - height).max(area.top));
    SetWindowPos(
        hwnd,
        null_mut(),
        x,
        y,
        width,
        height,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// First-run wizard: the surface depth is chosen first and the usage purposes
/// are part of the advanced answer only, so a simple choice never confronts a
/// new user with the detailed set.
unsafe fn show_usage_wizard(owner: HWND, app: App, settings: Settings) {
    let simple = UiMode::parse(&settings.ui_mode).is_simple();
    run_modal(
        owner,
        app,
        crate::i18n::ui("SSDownload kurulumu", "SSDownload setup"),
        576,
        412,
        DialogKind::Wizard(WizardFields {
            modes: settings.usage_modes,
            simple,
            video: null_mut(),
            file: null_mut(),
            audio: null_mut(),
            simple_card: null_mut(),
            advanced_card: null_mut(),
            start: null_mut(),
        }),
    );
}

/// Shows the settings dialog at the depth the selected mode asks for: the
/// advanced page exists only in the advanced mode, and the window is sized to
/// the rows that mode keeps, so a simple dialog is smaller instead of merely
/// emptier. Nothing is re-created: the rows of the hidden page stay in the
/// window and are simply not shown.
unsafe fn apply_settings_mode(dlg: &DialogUi, f: &SettingsFields) {
    let simple = UiMode::parse(&f.value.ui_mode).is_simple();
    let tabs = GetDlgItem(dlg.hwnd, S_TABS);
    let items = SendMessageW(tabs, TCM_GETITEMCOUNT, 0, 0) as i32;
    if simple {
        for index in (1..items).rev() {
            SendMessageW(tabs, TCM_DELETEITEM, 0, index as isize);
        }
        dlg.tab.set(0);
        SendMessageW(tabs, TCM_SETCURSEL, 0, 0);
    } else {
        if items < 2 {
            tab_item(tabs, 1, crate::i18n::ui("Gelişmiş", "Advanced"));
        }
        if items < 3 {
            tab_item(tabs, 2, crate::i18n::ui("Ağ", "Network"));
        }
        if items < 4 {
            tab_item(tabs, 3, crate::i18n::ui("Davranış", "Behaviour"));
        }
    }
    resize_dialog(
        dlg.hwnd,
        dlg.dpi,
        settings_logical_height(simple) + SETTINGS_FRAME,
    );
    layout_dialog(dlg);
}

unsafe fn show_add_dialog(owner: HWND, app: App, initial: String, analyze_first: bool) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Yeni indirme", "New download"),
        620,
        ADD_DIALOG_HEIGHT,
        DialogKind::Add(AddFields {
            urls: null_mut(),
            dir: null_mut(),
            filename: null_mut(),
            kind: null_mut(),
            kind_values: Vec::new(),
            connections: null_mut(),
            checksum: null_mut(),
            schedule: null_mut(),
            initial,
            analyze_first,
            advanced: false,
        }),
    );
}
/// Logical content height of the settings dialog for one surface depth: the
/// rows that depth shows plus the shared button and error rows above the
/// bottom edge. A simple dialog is the shorter one, because the rows it hides
/// are not replaced by anything.
fn settings_logical_height(simple: bool) -> i32 {
    if simple {
        372
    } else {
        398
    }
}
/// The frame of this dialog, measured once: the 346 logical content height it
/// shipped with was a 385 window. A mode's window is its own height plus this,
/// so neither mode opens with rows it does not have.
const SETTINGS_FRAME: i32 = 39;
/// The simple add dialog's window: its 292 logical content height plus the
/// shared frame, with a few pixels so DPI rounding never shows a scroll bar.
const ADD_DIALOG_HEIGHT: i32 = 292 + SETTINGS_FRAME + 3;

unsafe fn show_settings_dialog(owner: HWND, app: App, value: Settings) {
    let simple = UiMode::parse(&value.ui_mode).is_simple();
    run_modal(
        owner,
        app,
        crate::i18n::ui("SSDownload ayarları", "SSDownload settings"),
        560,
        settings_logical_height(simple) + SETTINGS_FRAME,
        DialogKind::Settings(Box::new(SettingsFields {
            value,
            dir: null_mut(),
            max_active: null_mut(),
            connections: null_mut(),
            media_fragments: null_mut(),
            per_host: null_mut(),
            speed: null_mut(),
            retry: null_mut(),
            clipboard: null_mut(),
            tray: null_mut(),
            startup: null_mut(),
            notify: null_mut(),
            schedule: null_mut(),
            start: null_mut(),
            end: null_mut(),
            dark: null_mut(),
            mode_video: null_mut(),
            mode_file: null_mut(),
            mode_audio: null_mut(),
            browser_transfer: null_mut(),
            update_check: null_mut(),
            log_level: null_mut(),
            log_hosts: null_mut(),
            ui_mode: null_mut(),
            site_entries: null_mut(),
            ui_language: null_mut(),
            proxy_mode: null_mut(),
            proxy_url: null_mut(),
            proxy_user: null_mut(),
            proxy_pass: null_mut(),
            speed_schedule: null_mut(),
            speed_start: null_mut(),
            speed_end: null_mut(),
            speed_kib: null_mut(),
            keep_awake: null_mut(),
            sound: null_mut(),
            takeover: null_mut(),
            completion_card: null_mut(),
            metered: null_mut(),
            hotkey: null_mut(),
            double_click: null_mut(),
            history: null_mut(),
        })),
    );
}
unsafe fn show_speed_limit(owner: HWND, app: App, ids: Vec<String>, initial: u64) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("İndirme hız sınırı", "Download speed limit"),
        420,
        150,
        DialogKind::Speed(SpeedFields {
            ids,
            initial,
            edit: null_mut(),
        }),
    );
}
unsafe fn show_rename(owner: HWND, app: App, id: String, name: String) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Yeniden adlandır", "Rename"),
        460,
        150,
        DialogKind::Rename(RenameFields {
            id,
            initial: name,
            edit: null_mut(),
        }),
    );
}

/// Opens a download's own window for already queued jobs (a card's "Büyüt").
pub(super) unsafe fn open_job_view(owner: HWND, app: App, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    let title = crate::i18n::ui("İndirme", "Download");
    show_modeless(
        owner,
        app,
        DialogRequest {
            title,
            width: 640,
            height: 420,
            kind: DialogKind::Progress(Box::new(ProgressFields {
                ids,
                selected: 0,
                raised: HashSet::new(),
                rendered: String::new(),
                rendered_rows: String::new(),
                action_label: String::new(),
                summary: null_mut(),
                list: null_mut(),
                status: null_mut(),
                progress: null_mut(),
                action: null_mut(),
                cancel: null_mut(),
                open_file: null_mut(),
                open_folder: null_mut(),
                close: null_mut(),
                hint: null_mut(),
                report: null_mut(),
            })),
            origin: None,
            launch: None,
            modeless: true,
        },
    );
}

unsafe fn show_site_logins(owner: HWND, app: App) {
    let logins = app.snapshot().settings.site_logins;
    run_modal(
        owner,
        app,
        crate::i18n::ui("Site girişleri", "Site logins"),
        700,
        470,
        DialogKind::Logins(LoginFields {
            logins,
            list: null_mut(),
            host: null_mut(),
            user: null_mut(),
            pass: null_mut(),
            allow_http: null_mut(),
        }),
    );
}
unsafe fn refresh_login_list(f: &LoginFields) {
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    for login in &f.logins {
        let line = format!(
            "{}  ·  {}{}",
            login.host,
            login.username,
            if login.allow_http { "  ·  HTTP" } else { "" }
        );
        SendMessageW(f.list, LB_ADDSTRING, 0, wide(&line).as_ptr() as LPARAM);
    }
}
unsafe fn show_queue_manager(owner: HWND, app: App, job_id: Option<String>) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Kuyruk yönetimi", "Queue manager"),
        700,
        470,
        DialogKind::Queues(QueueFields {
            list: null_mut(),
            ids: Vec::new(),
            name: null_mut(),
            concurrency: null_mut(),
            completion: null_mut(),
            countdown: null_mut(),
            enabled: null_mut(),
            windows: null_mut(),
            quota: null_mut(),
            program: null_mut(),
            arguments: null_mut(),
            delay: null_mut(),
            job_id,
        }),
    );
}
unsafe fn show_event_log(owner: HWND, app: App) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Olay günlüğü", "Event log"),
        700,
        470,
        DialogKind::Events(EventFields {
            list: null_mut(),
            rendered: Vec::new(),
        }),
    );
}
unsafe fn show_job_log(owner: HWND, app: App, job: &str, name: &str) {
    let title = if name.is_empty() {
        crate::i18n::ui("İş günlüğü", "Job log").to_string()
    } else {
        crate::i18n::ui_owned!(format!("İş günlüğü - {name}"), format!("Job log - {name}"))
    };
    run_modal(
        owner,
        app,
        &title,
        700,
        470,
        DialogKind::JobLog(JobLogFields {
            job: job.to_owned(),
            list: null_mut(),
            rendered: Vec::new(),
        }),
    );
}
unsafe fn show_rule_editor(owner: HWND, app: App) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Klasör kuralları", "Folder rules"),
        700,
        470,
        DialogKind::Rules(RuleFields {
            list: null_mut(),
            ids: Vec::new(),
            host: null_mut(),
            dir: null_mut(),
            subdomains: null_mut(),
            file: null_mut(),
            video: null_mut(),
            audio: null_mut(),
            priority: null_mut(),
        }),
    );
}
unsafe fn show_crawler(owner: HWND, app: App) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Site bağlantılarını tara", "Scan site links"),
        700,
        470,
        DialogKind::Crawler(CrawlerFields {
            url: null_mut(),
            depth: null_mut(),
            pages: null_mut(),
            candidates: null_mut(),
            list: null_mut(),
            result: None,
            pending: None,
        }),
    );
}
unsafe fn show_sync_manager(owner: HWND, app: App) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Periyodik dosya eşitleme", "Periodic file sync"),
        700,
        470,
        DialogKind::Synchronization(SyncFields {
            list: null_mut(),
            ids: Vec::new(),
            url: null_mut(),
            dir: null_mut(),
            interval: null_mut(),
            overwrite: null_mut(),
            enabled: null_mut(),
        }),
    );
}

unsafe fn show_tools_dialog(owner: HWND, app: App) {
    run_modal(
        owner,
        app,
        crate::i18n::ui("Medya araçları", "Media tools"),
        700,
        470,
        DialogKind::Tools(ToolsFields {
            status: null_mut(),
            progress: null_mut(),
            install: null_mut(),
            update: null_mut(),
            last_render: String::new(),
        }),
    );
}
unsafe fn show_media_dialog(owner: HWND, app: App, base: AddRequest) {
    if !app.snapshot().settings.usage_modes.allows_inspection() {
        message(
            owner,
            crate::i18n::ui(
                "Video veya ses indirmesi Ayarlar'da etkin değil.",
                "Video or audio downloading is disabled in Settings.",
            ),
            crate::i18n::ui("Video indir", "Video download"),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }
    let request = InspectRequest {
        request_id: None,
        url: base.url.clone(),
        headers: base.headers.clone(),
        session_cookies: base.session_cookies.clone(),
        referer: base.referer.clone(),
        page_url: base.page_url.clone(),
        playlist: base.playlist,
    };
    if let Err(e) = app.dispatch(Action::Inspect { request }) {
        message(
            owner,
            &crate::i18n::ui_owned!(
                format!("Analiz başlatılamadı:\n{e:#}"),
                format!("Analysis could not be started:\n{e:#}")
            ),
            crate::i18n::ui("Video indir", "Video download"),
            MB_OK | MB_ICONERROR,
        );
        return;
    }
    show_media_picker(owner, app, base, None, None, None);
}
/// Opens the picker for one media analysis: `session_consent` records whether
/// the handoff carried this site's session cookies, `launch` carries the handoff
/// this window belongs to and the repeat counter it starts from, and `origin` is
/// the monitor it came from.
unsafe fn show_media_picker(
    owner: HWND,
    app: App,
    mut base: AddRequest,
    session_consent: Option<bool>,
    origin: Option<[i32; 4]>,
    launch: Option<(u64, u64)>,
) {
    let settings = app.snapshot().settings.clone();
    let host = media_host(&base);
    let remembered = settings
        .site_media_defaults
        .iter()
        .find(|entry| !host.is_empty() && entry.host.eq_ignore_ascii_case(&host))
        .cloned();
    if let Some(entry) = &remembered {
        if entry.audio_only {
            base.kind = DownloadKind::Audio;
        } else {
            if !entry.container.is_empty() {
                base.container = Some(entry.container.clone());
            }
            base.max_height = entry.height;
        }
    }
    if base.container.is_none() {
        base.container = Some(settings.last_video_container);
    }
    if base.max_height.is_none() {
        base.max_height = settings.last_video_height;
    }
    if base.request_id.is_none() {
        base.request_id = Some(uuid::Uuid::new_v4().to_string());
    }
    let inspect_generation = app.snapshot().inspect_generation;
    let subtitle_none = app.subtitle_none();
    let picker = show_modeless(
        owner,
        app,
        DialogRequest {
            title: crate::i18n::ui("Biçim ve çözünürlük seç", "Choose format and resolution"),
            width: 640,
            height: 420,
            kind: DialogKind::Media(Box::new(MediaFields {
                base: base.clone(),
                subtitle_none,
                session_consent,
                summary: null_mut(),
                name: null_mut(),
                kind: null_mut(),
                kind_values: Vec::new(),
                format: null_mut(),
                audio: null_mut(),
                audio_tracks: null_mut(),
                subtitles: null_mut(),
                external_url: null_mut(),
                external_language: null_mut(),
                external_label: null_mut(),
                external_kind: null_mut(),
                external_default: null_mut(),
                external_list: null_mut(),
                full_verification: null_mut(),
                playlist: null_mut(),
                renew_job: null_mut(),
                renew_restart: null_mut(),
                renew_ids: Vec::new(),
                video_formats: Vec::new(),
                audio_values: Vec::new(),
                subtitle_values: Vec::new(),
                external_values: base.external_subtitles.clone(),
                loaded_title: String::new(),
                populated: false,
                inspect_generation,
                advanced: false,
                report_shown: false,
                report_error: None,
                host: host.clone(),
                auto_queue: remembered.as_ref().is_some_and(|entry| entry.auto),
                remember: null_mut(),
                playlist_items: null_mut(),
                thumbnail: null_mut(),
                thumbnail_requested: false,
            })),
            origin,
            launch,
            modeless: true,
        },
    );
    // Back where the user last left it, when that point is on the same monitor.
    if let Some([x, y]) = settings.picker_position {
        let point = POINT { x, y };
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONULL);
        let same = origin
            .map(|[l, t, r, b]| x >= l && x < r && y >= t && y < b)
            .unwrap_or(true);
        if !picker.is_null() && !monitor.is_null() && same {
            SetWindowPos(
                picker,
                null_mut(),
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// Host of the page a media request came from (else of the media itself).
fn media_host(request: &AddRequest) -> String {
    request
        .page_url
        .as_deref()
        .or(request.referer.as_deref())
        .and_then(|value| url::Url::parse(value).ok())
        .or_else(|| url::Url::parse(&request.url).ok())
        .and_then(|url| {
            url.host_str()
                .map(|host| host.trim_start_matches("www.").to_ascii_lowercase())
        })
        .unwrap_or_default()
}
/// One browser handoff: opens the picker on the originating monitor, ahead of
/// the browser that handed it off. The window takes the handoff over from here -
/// it raises itself when the same click repeats and ends the launch when it is
/// destroyed - so the next click starts a fresh analysis while a queued download
/// keeps running in its own window. Nothing here waits for the window: a
/// download that lasts minutes must not hold the browser handoffs or the
/// application.
unsafe fn open_media_launch(ui: &mut MainUi, launch: &MediaLaunch) {
    let base = launch.request.clone();
    show_media_picker(
        ui.hwnd,
        ui.app.clone(),
        base,
        Some(launch.session_consent),
        launch.origin,
        Some((launch.seq, launch.raise_seq)),
    );
}

unsafe extern "system" fn dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam as *const CREATESTRUCTW);
        let raw = cs.lpCreateParams as *mut DialogUi;
        (*raw).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
    }
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogUi;
    if raw.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    (*raw).callbacks += 1;
    let _callback = DialogCallback(raw);
    let dlg = &mut *raw;
    if let Some(value) = paint_theme(hwnd, msg, wparam, lparam) {
        return value;
    }
    match msg {
        WM_CREATE => {
            create_dialog_controls(dlg);
            0
        }
        WM_GETMINMAXINFO => {
            let info = &mut *(lparam as *mut MINMAXINFO);
            // Asked before WM_CREATE and again after a DPI change, so the DPI
            // comes from the window itself. The layout anchors its right-hand
            // columns, so shrinking below the design size would hide fields.
            let dpi = GetDpiForWindow(hwnd).max(96);
            let (x, y) = clamp_min_track(hwnd, dpi, dlg.design.0, dlg.design.1);
            info.ptMinTrackSize.x = x;
            info.ptMinTrackSize.y = y;
            0
        }
        WM_SIZE => {
            layout_dialog(dlg);
            0
        }
        WM_DPICHANGED => {
            let r = &*(lparam as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            dialog_dpi(dlg);
            0
        }
        WM_COMMAND => {
            dialog_command(dlg, loword(wparam) as i32, hiword(wparam));
            0
        }
        WM_MEASUREITEM => measure_tab_item(dlg, lparam as *mut MEASUREITEMSTRUCT) as LRESULT,
        WM_NOTIFY => {
            let header = &*(lparam as *const NMHDR);
            if header.code == TCN_SELCHANGE && header.idFrom == S_TABS as usize {
                let tabs = GetDlgItem(hwnd, S_TABS);
                let selected = SendMessageW(tabs, TCM_GETCURSEL, 0, 0).max(0) as usize;
                dlg.tab.set(selected);
                layout_dialog(dlg);
            }
            0
        }
        WM_VSCROLL => {
            let mut info: SCROLLINFO = zeroed();
            info.cbSize = size_of::<SCROLLINFO>() as u32;
            info.fMask = SIF_ALL;
            GetScrollInfo(hwnd, SB_VERT, &mut info);
            let y = match loword(wparam) as i32 {
                SB_LINEUP => info.nPos - scale(24, dlg.dpi),
                SB_LINEDOWN => info.nPos + scale(24, dlg.dpi),
                SB_PAGEUP => info.nPos - info.nPage as i32,
                SB_PAGEDOWN => info.nPos + info.nPage as i32,
                SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
                _ => info.nPos,
            };
            dlg.scroll_y.set(y.max(0));
            layout_dialog(dlg);
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // No system scroll bar in dark mode, so the keys scroll instead.
            let delta = scroll_delta_for_key(wparam as u16, dlg.dpi);
            if delta != 0 && dlg.scroll_range.get() > 0 {
                dlg.scroll_y
                    .set(dlg.scroll_y.get().saturating_add(delta).max(0));
                layout_dialog(dlg);
            }
            0
        }
        WM_PAINT => {
            let mut paint: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut paint);
            paint_scroll_thumb(dlg, dc);
            EndPaint(hwnd, &paint);
            0
        }
        WM_MOUSEWHEEL => {
            dlg.scroll_y.set(
                (dlg.scroll_y.get() - (hiword(wparam) as i16 as i32) * scale(48, dlg.dpi) / 120)
                    .max(0),
            );
            layout_dialog(dlg);
            0
        }
        WM_SETTINGCHANGE => {
            apply_dark_mode(hwnd, dlg.app.snapshot().settings.dark_mode);
            0
        }
        WM_DISPLAYCHANGE => {
            keep_window_on_screen(hwnd);
            dialog_dpi(dlg);
            0
        }
        WM_TIMER if wparam == TIMER_DIALOG => {
            dialog_timer(dlg);
            0
        }
        WM_CTLCOLORSTATIC => {
            SetTextColor(
                wparam as HDC,
                if lparam as HWND == dlg.error {
                    190 | (25 << 8) | (25 << 16)
                } else {
                    GetSysColor(COLOR_WINDOWTEXT)
                },
            );
            SetBkMode(wparam as HDC, TRANSPARENT as i32);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_THUMBNAIL => {
            let bitmap = lparam as HBITMAP;
            if let DialogKind::Media(f) = &mut dlg.kind {
                let previous = SendMessageW(
                    f.thumbnail,
                    STM_SETIMAGE,
                    IMAGE_BITMAP as usize,
                    bitmap as LPARAM,
                );
                if previous != 0 {
                    DeleteObject(previous as HGDIOBJ);
                }
                ShowWindow(f.thumbnail, SW_SHOW);
                layout_dialog(dlg);
            } else {
                DeleteObject(bitmap as HGDIOBJ);
            }
            0
        }
        WM_DESTROY => {
            if let DialogKind::Media(f) = &dlg.kind {
                // The picker opens where it was last left; its bitmap is released.
                let image = SendMessageW(f.thumbnail, STM_GETIMAGE, IMAGE_BITMAP as usize, 0);
                if image != 0 {
                    DeleteObject(image as HGDIOBJ);
                }
                let mut rect: RECT = zeroed();
                if IsIconic(hwnd) == 0 && GetWindowRect(hwnd, &mut rect) != 0 {
                    let mut settings = dlg.app.snapshot().settings;
                    let value = Some([rect.left, rect.top]);
                    if settings.picker_position != value {
                        settings.picker_position = value;
                        let _ = dlg.app.update_settings_quiet(settings);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCDESTROY => {
            KillTimer(hwnd, TIMER_DIALOG);
            if !dlg.font.is_null() {
                DeleteObject(dlg.font);
                dlg.font = null_mut();
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            // This window was the current browser handoff: it ends with the
            // window, so a later click on the same page starts a new analysis
            // instead of being raised into a window that is gone. The native
            // sequence leaves a newer handoff alone, even if its browser id repeats.
            if let Some(launch) = dlg.launch.take() {
                dlg.app.clear_media_launch(launch);
            }
            dlg.destroyed = true;
            // Modal state belongs to run_modal_at. Modeless state is released
            // by the final callback, including any handler below a nested loop.
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn dialog_label(parent: HWND, font: HFONT, label: &str, id: i32) -> HWND {
    let id = if id == 0 {
        5000 + child_statics(parent, null_mut()).len() as i32
    } else {
        id
    };
    let h = control(parent, "STATIC", label, WS_CHILD | WS_VISIBLE, 0, id);
    apply_font(h, font);
    h
}
unsafe fn dialog_edit(parent: HWND, font: HFONT, id: i32, multiline: bool, number: bool) -> HWND {
    let mut s = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32;
    if multiline {
        s |= ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32 | WS_VSCROLL;
    }
    if number {
        s |= ES_NUMBER as u32;
    }
    let h = control(parent, "EDIT", "", s, WS_EX_CLIENTEDGE, id);
    apply_font(h, font);
    h
}
/// Append one tab to a tab control.
unsafe fn tab_item(tabs: HWND, index: i32, label: &str) {
    let mut text = wide(label);
    let mut item: TCITEMW = zeroed();
    item.mask = TCIF_TEXT;
    item.pszText = text.as_mut_ptr();
    SendMessageW(
        tabs,
        TCM_INSERTITEMW,
        index as usize,
        &item as *const _ as LPARAM,
    );
}
unsafe fn dialog_button(parent: HWND, font: HFONT, label: &str, id: i32, default: bool) -> HWND {
    let h = control(
        parent,
        "BUTTON",
        label,
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | if default {
                BS_DEFPUSHBUTTON as u32
            } else {
                BS_PUSHBUTTON as u32
            },
        0,
        id,
    );
    apply_font(h, font);
    h
}
unsafe fn dialog_check(parent: HWND, font: HFONT, label: &str, id: i32) -> HWND {
    let h = control(
        parent,
        "BUTTON",
        label,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
        0,
        id,
    );
    apply_font(h, font);
    h
}
/// Checkbox whose label wraps onto a second line instead of clipping, for
/// settings whose explanation is longer than the dialog body.
unsafe fn dialog_check_wrapped(parent: HWND, font: HFONT, label: &str, id: i32) -> HWND {
    let h = control(
        parent,
        "BUTTON",
        label,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32,
        0,
        id,
    );
    apply_font(h, font);
    h
}
unsafe fn dialog_list(parent: HWND, font: HFONT, id: i32, multi: bool) -> HWND {
    let h = control(
        parent,
        "LISTBOX",
        "",
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_BORDER
            | WS_VSCROLL
            | LBS_NOINTEGRALHEIGHT as u32
            | if multi { LBS_EXTENDEDSEL as u32 } else { 0 },
        WS_EX_CLIENTEDGE,
        id,
    );
    apply_font(h, font);
    h
}

unsafe fn dialog_combo(parent: HWND, font: HFONT, id: i32) -> HWND {
    let h = control(
        parent,
        "COMBOBOX",
        "",
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
        0,
        id,
    );
    apply_font(h, font);
    h
}
/// The wizard can only finish on a complete answer: the mode is always chosen
/// (its two cards are exclusive), and in advanced mode at least one purpose
/// must be selected.
unsafe fn wizard_start_enabled(f: &WizardFields) -> bool {
    if check(f.simple_card) {
        return true;
    }
    check(f.video) || check(f.file) || check(f.audio)
}

unsafe fn create_error(dlg: &mut DialogUi) {
    dlg.error = dialog_label(dlg.hwnd, dlg.font, "", D_ERROR);
}

unsafe fn create_dialog_controls(dlg: &mut DialogUi) {
    dlg.dpi = GetDpiForWindow(dlg.hwnd).max(96);
    dlg.font = make_font(dlg.dpi, 9, FW_NORMAL as i32);
    create_error(dlg);
    create_kind_controls(dlg);
    apply_dark_mode(dlg.hwnd, dlg.app.snapshot().settings.dark_mode);
    layout_dialog(dlg);
    dialog_timer(dlg);
}

/// Re-creates a window's controls for the kind it now is. A picker that has
/// submitted its selection has no selection left to hold: those controls are
/// destroyed with the state that owned them, and the job view is built in their
/// place, in the same window and on the same monitor.
unsafe fn rebuild_dialog_controls(dlg: &mut DialogUi) {
    let mut child = GetWindow(dlg.hwnd, GW_CHILD);
    while !child.is_null() {
        let next = GetWindow(child, GW_HWNDNEXT);
        DestroyWindow(child);
        child = next;
    }
    if !dlg.font.is_null() {
        DeleteObject(dlg.font);
        dlg.font = null_mut();
    }
    dlg.error = null_mut();
    dlg.scroll_y.set(0);
    dlg.scroll_range.set(0);
    create_dialog_controls(dlg);
}

/// Turns a picker into the job view of the download it just queued: the window
/// keeps its place, its handoff and its size, and its title names what it is
/// now showing.
unsafe fn begin_job_view(dlg: &mut DialogUi, job_ids: Vec<String>) {
    let snapshot = dlg.app.snapshot();
    let title = match job_ids.as_slice() {
        [id] => snapshot
            .jobs
            .iter()
            .find(|job| &job.id == id)
            .map(|job| {
                crate::i18n::ui_owned!(
                    format!("İndirme — {}", job.name),
                    format!("Download — {}", job.name)
                )
            })
            .unwrap_or_else(|| crate::i18n::ui("İndirme", "Download").to_string()),
        ids => crate::i18n::ui_owned!(
            format!("İndirme — {} öğe", ids.len()),
            format!("Download — {} items", ids.len())
        ),
    };
    dlg.kind = DialogKind::Progress(Box::new(ProgressFields {
        ids: job_ids,
        selected: 0,
        raised: HashSet::new(),
        rendered: String::new(),
        rendered_rows: String::new(),
        action_label: String::new(),
        summary: null_mut(),
        list: null_mut(),
        status: null_mut(),
        progress: null_mut(),
        action: null_mut(),
        cancel: null_mut(),
        open_file: null_mut(),
        open_folder: null_mut(),
        close: null_mut(),
        hint: null_mut(),
        report: null_mut(),
    }));
    SetWindowTextW(dlg.hwnd, wide(&title).as_ptr());
    // The picker may have been unfolded to its advanced geometry; the job view
    // is one page, so it goes back to the size the window opens with.
    resize_dialog(dlg.hwnd, dlg.dpi, 420);
    rebuild_dialog_controls(dlg);
}

unsafe fn refresh_queue_controls(
    hwnd: HWND,
    f: &mut QueueFields,
    snapshot: &AppSnapshot,
) -> anyhow::Result<()> {
    let prior = SendMessageW(f.list, LB_GETCURSEL, 0, 0) as i32;
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    f.ids = snapshot
        .settings
        .queues
        .iter()
        .map(|q| q.id.clone())
        .collect();
    for queue in &snapshot.settings.queues {
        let value = wide(&format!(
            "{}{}",
            queue.name,
            if queue.enabled {
                ""
            } else {
                crate::i18n::ui(" (kapalı)", " (off)")
            }
        ));
        SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
    if !snapshot.settings.queues.is_empty() {
        let index = prior.clamp(0, snapshot.settings.queues.len() as i32 - 1);
        SendMessageW(f.list, LB_SETCURSEL, index as usize, 0);
        let queue = &snapshot.settings.queues[index as usize];
        populate_queue_fields(f, queue)?;
    }
    refresh_completion_countdown(hwnd, f, snapshot);
    Ok(())
}
unsafe fn populate_queue_fields(f: &QueueFields, queue: &QueuePolicy) -> anyhow::Result<()> {
    set_text(f.name, &queue.name);
    set_text(f.concurrency, &queue.concurrency.to_string());
    set_check(f.enabled, queue.enabled);
    set_text(
        f.windows,
        &queue
            .windows
            .iter()
            .map(|window| {
                format!(
                    "{} {}-{}",
                    if window.weekdays.is_empty() {
                        "*".into()
                    } else {
                        window
                            .weekdays
                            .iter()
                            .map(u8::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    },
                    window.start,
                    window.end
                )
            })
            .collect::<Vec<_>>()
            .join("\r\n"),
    );
    set_text(
        f.quota,
        &queue
            .quota
            .as_ref()
            .map(|q| q.limit_bytes)
            .unwrap_or(0)
            .to_string(),
    );
    let (choice, delay, program, args) = match &queue.completion {
        CompletionAction::None => (0, 30, String::new(), "[]".into()),
        CompletionAction::Notify => (1, 30, String::new(), "[]".into()),
        CompletionAction::ShutdownComputer { countdown_seconds } => {
            (2, *countdown_seconds, String::new(), "[]".into())
        }
        CompletionAction::RunProgram {
            program,
            arguments,
            countdown_seconds,
        } => (
            3,
            *countdown_seconds,
            program.to_string_lossy().into_owned(),
            serde_json::to_string(arguments)?,
        ),
    };
    combo_select(f.completion, choice);
    set_text(f.delay, &delay.to_string());
    set_text(f.program, &program);
    set_text(f.arguments, &args);
    Ok(())
}
unsafe fn refresh_completion_countdown(hwnd: HWND, f: &QueueFields, snapshot: &AppSnapshot) {
    let quota = selected_management_id(f.list, &f.ids)
        .and_then(|id| snapshot.settings.queues.iter().find(|q| q.id == id))
        .and_then(|q| q.quota.as_ref())
        .map(|q| {
            crate::i18n::ui_owned!(
                format!(
                    "Kalan kota: {} / {}. ",
                    format_bytes(q.limit_bytes.saturating_sub(q.consumed_bytes)),
                    format_bytes(q.limit_bytes)
                ),
                format!(
                    "Remaining quota: {} / {}. ",
                    format_bytes(q.limit_bytes.saturating_sub(q.consumed_bytes)),
                    format_bytes(q.limit_bytes)
                )
            )
        })
        .unwrap_or_default();
    if let Some(countdown) = &snapshot.completion_countdown {
        let remaining = (countdown.deadline - chrono::Utc::now().timestamp()).max(0);
        set_text(
            f.countdown,
            &crate::i18n::ui_owned!(
                format!(
                    "{quota}Bitiş eylemi {remaining} saniye içinde çalışacak; iptal edebilirsiniz."
                ),
                format!(
                "{quota}The completion action will run in {remaining} seconds; you can cancel it."
            )
            ),
        );
        EnableWindow(GetDlgItem(hwnd, Q_CANCEL_COMPLETION), 1);
    } else {
        set_text(
            f.countdown,
            &crate::i18n::ui_owned!(
                format!("{quota}Etkin bitiş geri sayımı yok."),
                format!("{quota}No completion countdown is active.")
            ),
        );
        EnableWindow(GetDlgItem(hwnd, Q_CANCEL_COMPLETION), 0);
    }
}
unsafe fn refresh_rule_list(f: &mut RuleFields, rules: &[FolderRule]) {
    f.ids = rules.iter().map(|r| r.id.clone()).collect();
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    for rule in rules {
        let value = wide(&format!("{} → {}", rule.host, rule.destination.display()));
        SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
}
unsafe fn refresh_sync_list(f: &mut SyncFields, policies: &[SyncPolicy]) {
    f.ids = policies.iter().map(|p| p.id.clone()).collect();
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    for policy in policies {
        let value = wide(&format!(
            "{} → {}{}",
            policy.url,
            policy.destination.display(),
            policy
                .last_error
                .as_ref()
                .map(|_| crate::i18n::ui(" · hata", " · error"))
                .unwrap_or("")
        ));
        SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
}
unsafe fn refresh_event_list(f: &mut EventFields) {
    let lines = crate::logging::recent(200);
    if lines == f.rendered {
        return;
    }
    f.rendered = lines;
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    for line in &f.rendered {
        let value = wide(line);
        SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
    if !f.rendered.is_empty() {
        SendMessageW(f.list, LB_SETTOPINDEX, f.rendered.len() - 1, 0);
    }
}
/// Fills the list from the download's own timeline file. Compared before redrawing, so an open
/// window keeps its scroll position while nothing new arrives.
unsafe fn refresh_job_log(f: &mut JobLogFields) {
    let lines = crate::logging::job_timeline(&f.job, 400);
    let rendered = if lines.is_empty() {
        vec![crate::i18n::ui(
            "Bu indirme için henüz kayıt yok.",
            "No entries for this download yet.",
        )
        .to_string()]
    } else {
        lines
    };
    if rendered == f.rendered {
        return;
    }
    f.rendered = rendered;
    SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
    for line in &f.rendered {
        let value = wide(line);
        SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
    if !f.rendered.is_empty() {
        SendMessageW(f.list, LB_SETTOPINDEX, f.rendered.len() - 1, 0);
    }
}

/// Puts text on the clipboard as CF_UNICODETEXT, so a log can be pasted into a report.
unsafe fn set_clipboard_text(value: &str) -> bool {
    if OpenClipboard(null_mut()) == 0 {
        return false;
    }
    let mut ok = false;
    if EmptyClipboard() != 0 {
        let units = value.encode_utf16().count();
        let bytes = (units + 1) * 2;
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !handle.is_null() {
            let ptr = GlobalLock(handle) as *mut u16;
            if !ptr.is_null() {
                for (index, unit) in value.encode_utf16().enumerate() {
                    *ptr.add(index) = unit;
                }
                *ptr.add(units) = 0;
                GlobalUnlock(handle);
                if SetClipboardData(CF_UNICODETEXT_, handle).is_null() {
                    GlobalFree(handle);
                } else {
                    ok = true;
                }
            } else {
                GlobalFree(handle);
            }
        }
    }
    CloseClipboard();
    ok
}

unsafe fn open_folder(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let Ok(target) = crate::winpath::shell_target(path) else {
        return false;
    };
    crate::logging::record(
        crate::logging::Event::info("shell.open_folder").detail(format!("path={target}")),
    );
    ShellExecuteW(
        null_mut(),
        wide("open").as_ptr(),
        wide(&target).as_ptr(),
        null(),
        null(),
        SW_SHOWNORMAL,
    ) as isize
        > 32
}

unsafe fn move_id(hwnd: HWND, id: i32, x: i32, y: i32, w: i32, h: i32) {
    let c = GetDlgItem(hwnd, id);
    if !c.is_null() {
        MoveWindow(c, x, y, w, h, 1);
    }
}
/// Position a control and toggle it with its tab.
unsafe fn place(control: HWND, visible: bool, x: i32, y: i32, w: i32, h: i32) {
    if control.is_null() {
        return;
    }
    ShowWindow(control, if visible { SW_SHOW } else { SW_HIDE });
    MoveWindow(control, x, y, w, h, 1);
}
/// Position a label static by its creation index and toggle it with its tab.
unsafe fn place_static(
    labels: &[HWND],
    index: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    visible: bool,
) {
    if let Some(&label) = labels.get(index) {
        place(label, visible, x, y, w, h);
    }
}
/// The scroll step a key asks for, zero when the key is not a scroll key.
fn scroll_delta_for_key(key: u16, dpi: u32) -> i32 {
    let step = scale(48, dpi);
    let page = scale(120, dpi);
    match key {
        VK_UP => -step,
        VK_DOWN => step,
        VK_PRIOR => -page,
        VK_NEXT => page,
        VK_HOME => i32::MIN / 2,
        VK_END => i32::MAX / 2,
        _ => 0,
    }
}

/// The dark scroll affordance: a slim thumb on the right edge, sized and
/// placed from the scroll range and offset the layout computed.
unsafe fn paint_scroll_thumb(dlg: &DialogUi, dc: HDC) {
    let range = dlg.scroll_range.get();
    if range <= 0 || !dialog_is_dark(dlg) {
        return;
    }
    let mut client: RECT = zeroed();
    if GetClientRect(dlg.hwnd, &mut client) == 0 {
        return;
    }
    let margin = scale(DIALOG_MARGIN, dlg.dpi);
    let row = scale(DIALOG_ROW, dlg.dpi);
    let width = scale(4, dlg.dpi).max(2);
    let track_top = client.top + margin;
    let track_bottom = (client.bottom - margin - row).max(track_top + width);
    let track = track_bottom - track_top;
    let content = (client.bottom - client.top) + range;
    let height = ((track as i64 * (client.bottom - client.top) as i64) / content.max(1) as i64)
        .max(width as i64) as i32;
    let travel = (track - height).max(0);
    let offset = dlg.scroll_y.get().clamp(0, range);
    let top = track_top + ((travel as i64 * offset as i64) / range.max(1) as i64) as i32;
    let thumb = RECT {
        left: client.right - width - scale(2, dlg.dpi),
        top,
        right: client.right - scale(2, dlg.dpi),
        bottom: top + height,
    };
    FillRect(dc, &thumb, palette_brush(DARK_MUTED));
}

unsafe fn child_statics(parent: HWND, error: HWND) -> Vec<HWND> {
    let mut out = Vec::<HWND>::new();
    let mut child = GetWindow(parent, GW_CHILD);
    while !child.is_null() {
        if window_class(child).eq_ignore_ascii_case("Static") && child != error {
            out.push(child);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    out.sort_by_key(|hwnd| GetDlgCtrlID(*hwnd));
    out
}
/// Class name of a window, trimmed of the terminator.
unsafe fn window_class(hwnd: HWND) -> String {
    let mut name = [0u16; 64];
    let len = GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&name[..len])
}
unsafe fn dialog_dpi(dlg: &mut DialogUi) {
    dlg.dpi = GetDpiForWindow(dlg.hwnd).max(96);
    let f = make_font(dlg.dpi, 9, FW_NORMAL as i32);
    let mut child = GetWindow(dlg.hwnd, GW_CHILD);
    while !child.is_null() {
        apply_font(child, f);
        child = GetWindow(child, GW_HWNDNEXT);
    }
    DeleteObject(dlg.font);
    dlg.font = f;
    layout_dialog(dlg);
}

unsafe fn set_dialog_error(hwnd: HWND, error: HWND, value: &str, focus: HWND) {
    // This line is read by the user, so it names the tools the way the rest of
    // the surface does; the dialog's own state knows which surface that is.
    let state = dialog_state(hwnd);
    let debug = !state.is_null() && (*state).app.snapshot().settings.debug_mode;
    set_text(error, &display_text(debug, value));
    if !focus.is_null() {
        SetFocus(focus);
    }
    InvalidateRect(hwnd, null(), 1);
}
fn parse_u8_range(value: &str, min: u8, max: u8, label: &str) -> std::result::Result<u8, String> {
    let n = value.trim().parse::<u8>().map_err(|_| {
        crate::i18n::ui_owned!(
            format!("{label} bir sayı olmalıdır."),
            format!("{label} must be a number.")
        )
    })?;
    if n < min || n > max {
        Err(crate::i18n::ui_owned!(
            format!("{label} {min}–{max} arasında olmalıdır."),
            format!("{label} must be between {min} and {max}.")
        ))
    } else {
        Ok(n)
    }
}
fn valid_clock(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && value[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && value[3..].parse::<u8>().is_ok_and(|m| m < 60)
}
unsafe fn collect_add(
    f: &AddFields,
    single_required: bool,
) -> std::result::Result<Vec<AddRequest>, (String, HWND)> {
    let raw = text(f.urls);
    let mut urls = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A batch range (`img[001-120].jpg`) expands into its addresses first.
        match crate::validation::expand_batch_pattern(line) {
            Ok(Some(expanded)) => {
                for url in expanded {
                    if crate::validation::validate_url(&url, false).is_ok() {
                        urls.push(url);
                    }
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => return Err((error.to_string(), f.urls)),
        }
        if crate::validation::validate_url(line, false).is_ok() {
            urls.push(line.to_string());
        } else {
            urls.extend(extract_urls(line));
        }
    }
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
    if urls.is_empty() {
        return Err((
            crate::i18n::ui(
                "En az bir geçerli HTTP/HTTPS/FTP URL girin.",
                "Enter at least one valid HTTP/HTTPS/FTP URL.",
            )
            .to_string(),
            f.urls,
        ));
    }
    if single_required && urls.len() != 1 {
        return Err((
            crate::i18n::ui(
                "Video indir için tek bir URL girin.",
                "Enter a single URL to download a video.",
            )
            .to_string(),
            f.urls,
        ));
    }
    let filename = text(f.filename).trim().to_string();
    if urls.len() > 1 && !filename.is_empty() {
        return Err((
            crate::i18n::ui(
                "Birden çok URL için özel dosya adı kullanılamaz.",
                "A custom file name cannot be used for multiple URLs.",
            )
            .to_string(),
            f.filename,
        ));
    }
    if filename.chars().any(|c| matches!(c, '\\' | '/')) {
        return Err((
            crate::i18n::ui(
                "Dosya adı klasör ayırıcı içeremez.",
                "The file name cannot contain a folder separator.",
            )
            .to_string(),
            f.filename,
        ));
    }
    let conn = if text(f.connections).trim().is_empty() {
        None
    } else {
        Some(
            parse_u8_range(
                &text(f.connections),
                1,
                16,
                crate::i18n::ui("Bağlantı sayısı", "Connection count"),
            )
            .map_err(|e| (e, f.connections))?,
        )
    };
    let checksum = text(f.checksum).trim().to_ascii_lowercase();
    if !checksum.is_empty()
        && (checksum.len() != 64 || !checksum.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err((
            crate::i18n::ui(
                "SHA-256 değeri tam 64 onaltılık karakter olmalıdır.",
                "The SHA-256 value must be exactly 64 hexadecimal characters.",
            )
            .to_string(),
            f.checksum,
        ));
    }
    let schedule = text(f.schedule).trim().to_string();
    let start_at = if schedule.is_empty() {
        None
    } else {
        let n = NaiveDateTime::parse_from_str(&schedule, "%Y-%m-%d %H:%M").map_err(|_| {
            (
                crate::i18n::ui(
                    "Zamanlama biçimi YYYY-AA-GG SS:DD olmalıdır.",
                    "The schedule must be in YYYY-MM-DD HH:MM format.",
                )
                .to_string(),
                f.schedule,
            )
        })?;
        Some(
            Local
                .from_local_datetime(&n)
                .single()
                .ok_or_else(|| {
                    (
                        crate::i18n::ui("Bu yerel saat yaz saati geçişi nedeniyle belirsiz/geçersiz.", "This local time is ambiguous/invalid because of a daylight saving transition.").to_string(),
                        f.schedule,
                    )
                })?
                .timestamp(),
        )
    };
    let kind = f
        .kind_values
        .get(combo_index(f.kind).max(0) as usize)
        .copied()
        .unwrap_or(DownloadKind::Auto);
    let dir = text(f.dir).trim().to_string();
    Ok(urls
        .into_iter()
        .map(|url| AddRequest {
            url,
            kind,
            filename: (!filename.is_empty()).then(|| filename.clone()),
            directory: (!dir.is_empty()).then(|| PathBuf::from(&dir)),
            connections: conn,
            checksum: (!checksum.is_empty()).then(|| checksum.clone()),
            start_at,
            ..Default::default()
        })
        .collect())
}

unsafe fn selected_management_id(list: HWND, ids: &[String]) -> Option<&str> {
    let index = SendMessageW(list, LB_GETCURSEL, 0, 0);
    if index < 0 {
        None
    } else {
        ids.get(index as usize).map(String::as_str)
    }
}
unsafe fn resize_dialog(hwnd: HWND, dpi: u32, height: i32) {
    let mut rect: RECT = zeroed();
    GetWindowRect(hwnd, &mut rect);
    let width = rect.right - rect.left;
    let tall = scale(height, dpi);
    // The dialog keeps the centre of its current monitor: a handoff picker
    // stays on the browser's monitor when its advanced rows open. Only a
    // window that no longer fits is re-anchored on the owner.
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info: MONITORINFO = zeroed();
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) != 0 {
        let area = info.rcWork;
        let x = ((rect.left + rect.right) / 2 - width / 2)
            .clamp(area.left, (area.right - width).max(area.left));
        let y = ((rect.top + rect.bottom) / 2 - tall / 2)
            .clamp(area.top, (area.bottom - tall).max(area.top));
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            width,
            tall,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        return;
    }
    SetWindowPos(
        hwnd,
        null_mut(),
        0,
        0,
        width,
        tall,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    center_window(hwnd, GetWindow(hwnd, GW_OWNER));
}
// IFileDialog / IShellItem ABI, from Windows SDK shobjidl_core.h.
#[repr(C)]
struct ShellObject {
    vtable: *const *const c_void,
}
struct ShellPtr(*mut ShellObject);
impl Drop for ShellPtr {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                let release: unsafe extern "system" fn(*mut ShellObject) -> u32 =
                    std::mem::transmute(*(*self.0).vtable.add(2));
                release(self.0);
            }
        }
    }
}
unsafe fn browse_folder(owner: HWND, initial: &str) -> Option<PathBuf> {
    use windows_sys::{
        core::GUID,
        Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
    };
    let clsid = GUID::from_u128(0xdc1c5a9c_e88a_4dde_a5a1_60f82a20aef7);
    let iid = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
    let shell_iid = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);
    let mut ptr = null_mut();
    if CoCreateInstance(&clsid, null_mut(), CLSCTX_INPROC_SERVER, &iid, &mut ptr) < 0 {
        return None;
    }
    let dialog = ShellPtr(ptr.cast());
    let object = dialog.0;
    let get_options: unsafe extern "system" fn(*mut ShellObject, *mut u32) -> i32 =
        std::mem::transmute(*(*object).vtable.add(10));
    let set_options: unsafe extern "system" fn(*mut ShellObject, u32) -> i32 =
        std::mem::transmute(*(*object).vtable.add(9));
    let show: unsafe extern "system" fn(*mut ShellObject, HWND) -> i32 =
        std::mem::transmute(*(*object).vtable.add(3));
    let set_folder: unsafe extern "system" fn(*mut ShellObject, *mut ShellObject) -> i32 =
        std::mem::transmute(*(*object).vtable.add(12));
    let get_result: unsafe extern "system" fn(*mut ShellObject, *mut *mut ShellObject) -> i32 =
        std::mem::transmute(*(*object).vtable.add(20));
    let mut options = 0;
    if get_options(object, &mut options) < 0
        || set_options(
            object,
            options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
        ) < 0
    {
        return None;
    }
    if !initial.is_empty() {
        let mut folder = null_mut();
        if SHCreateItemFromParsingName(wide(initial).as_ptr(), null_mut(), &shell_iid, &mut folder)
            >= 0
        {
            let folder = ShellPtr(folder.cast());
            set_folder(object, folder.0);
        }
    }
    if show(object, owner) < 0 {
        return None;
    }
    let mut item = null_mut();
    if get_result(object, &mut item) < 0 {
        return None;
    }
    let item = ShellPtr(item);
    let display: unsafe extern "system" fn(*mut ShellObject, i32, *mut *mut u16) -> i32 =
        std::mem::transmute(*(*item.0).vtable.add(5));
    let mut path = null_mut();
    if display(item.0, SIGDN_FILESYSPATH, &mut path) < 0 || path.is_null() {
        return None;
    }
    let mut len = 0;
    while *path.add(len) != 0 {
        len += 1;
    }
    let value = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
        path, len,
    )));
    CoTaskMemFree(path.cast());
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{subtitle_default_index, subtitle_default_selected};
    use crate::model::SubtitleSelection;

    fn track(language: &str, automatic: bool) -> crate::model::SubtitleTrack {
        crate::model::SubtitleTrack {
            language: language.into(),
            automatic,
        }
    }

    #[test]
    fn state_label_shows_a_phase_only_when_it_adds_something() {
        use crate::model::{AddRequest, Job, JobState};
        let job = |state: JobState, phase: &str| Job {
            id: "job".into(),
            request: AddRequest::default(),
            name: "file.bin".into(),
            state,
            path: "file.bin".into(),
            downloaded: 0,
            total: None,
            speed: 0,
            eta: None,
            error: None,
            phase: phase.into(),
            created_at: 0,
            updated_at: 0,
            attempts: 0,
            work_dir: None,
            remove_requested: None,
            claim: None,
            legacy_completed: false,
            browser_transfer_authorized: false,
            priority: 0,
            force_start: false,
            open_when_done: false,
        };
        let downloading = JobState::Downloading.label();
        assert_eq!(
            super::state_label(&job(JobState::Downloading, downloading)),
            downloading
        );
        assert_eq!(
            super::state_label(&job(JobState::Downloading, " ")),
            downloading
        );
        assert_eq!(
            super::state_label(&job(JobState::Processing, "MP4")),
            format!("{} — MP4", JobState::Processing.label())
        );
        assert_eq!(
            super::state_label(&job(JobState::Paused, "MP4")),
            JobState::Paused.label()
        );
    }

    #[test]
    fn user_facing_text_never_names_the_managed_components_outside_developer_mode() {
        let raw = "yt-dlp sürüm bilgisi alınamadı: ffprobe ve FFmpeg denetlenemedi, Deno eksik";
        let shown = super::display_text(false, raw);
        for name in ["yt-dlp", "ffprobe", "FFmpeg", "Deno"] {
            assert!(
                !shown.contains(name),
                "{name} kullanıcı metnine sızdı: {shown}"
            );
        }
        // ffprobe is its own word, not a fragment of the FFmpeg rule, and the
        // rest of the sentence is untouched.
        assert!(shown.contains("dönüştürücü ve dönüştürücü"), "{shown}");
        assert!(shown.contains("sürüm bilgisi alınamadı"), "{shown}");
        // The developer surface keeps the upstream names verbatim.
        assert_eq!(super::display_text(true, raw), raw);
    }

    #[test]
    fn subtitle_default_prefers_original_captions_and_does_not_force_live_captions() {
        let captions = [track("aa", true), track("tr", true), track("tr-orig", true)];
        let default = subtitle_default_index(&captions, false);
        let selected: Vec<_> = captions
            .iter()
            .enumerate()
            .filter(|(index, track)| {
                subtitle_default_selected(false, &[], track, default == Some(*index))
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(selected, [2]);
        assert_eq!(subtitle_default_index(&captions, true), None);
        let manual = [track("en", false), track("tr-orig", true)];
        assert_eq!(subtitle_default_index(&manual, false), Some(0));
        assert_eq!(subtitle_default_index(&[], false), None);
        let explicit = vec![SubtitleSelection {
            language: "aa".into(),
            automatic: true,
        }];
        assert!(subtitle_default_selected(
            false,
            &explicit,
            &captions[0],
            false
        ));
    }

    #[test]
    fn subtitle_default_preselects_one_track_and_respects_explicit_choices() {
        let first = track("tr", false);
        let second = track("en", false);
        let third = track("de", true);

        assert!(subtitle_default_selected(false, &[], &first, true));
        assert!(!subtitle_default_selected(false, &[], &second, false));
        assert!(!subtitle_default_selected(false, &[], &third, false));

        assert!(!subtitle_default_selected(true, &[], &first, true));

        let previous = vec![SubtitleSelection {
            language: "en".into(),
            automatic: false,
        }];
        assert!(!subtitle_default_selected(false, &previous, &first, true));
        assert!(subtitle_default_selected(false, &previous, &second, false));
        assert!(!subtitle_default_selected(false, &previous, &third, false));
    }
}
