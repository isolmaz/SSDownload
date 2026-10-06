//! Creation of every dialog kind's controls.

use super::*;

/// The controls one kind of dialog is made of, plus the timer that keeps that
/// kind up to date. Created once when the window is created, and again when a
/// window changes kind in place (`rebuild_dialog_controls`).
pub(super) unsafe fn create_kind_controls(dlg: &mut DialogUi) {
    let dlg_ptr = dlg as *mut DialogUi;
    match &mut (*dlg_ptr).kind {
        DialogKind::Wizard(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Nasıl kullanmak istediğinizi seçin. Bu seçimi daha sonra Ayarlar'dan değiştirebilirsiniz.",
                    "Choose how you want to use it. You can change this later in Settings.",
                ),
                0,
            );
            f.simple_card = dialog_card(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Basit — indirme listesi, yeni indirme ve temel ayarlar",
                    "Basic — downloads, new download and essential settings",
                ),
                ID_WIZARD_SIMPLE,
            );
            f.advanced_card = dialog_card(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Gelişmiş — kuyruk, klasör kuralları, site gezgini, eşitleme, günlükler",
                    "Advanced — queues, folder rules, site crawler, sync, logs",
                ),
                ID_WIZARD_ADVANCED,
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Kullanım amaçları (Gelişmiş modda geçerlidir):",
                    "Usage modes (applies in Advanced mode):",
                ),
                0,
            );
            f.video = dialog_card(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Video — MP4/MKV, kalite, ses parçası ve altyazı seçenekleri",
                    "Video — MP4/MKV, quality, audio track and subtitle options",
                ),
                ID_WIZARD_VIDEO,
            );
            f.file = dialog_card(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Dosya — normal dosya bağlantılarını güvenilir biçimde aktar",
                    "File — reliably transfer regular file links",
                ),
                ID_WIZARD_FILE,
            );
            f.audio = dialog_card(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Ses — video sayfalarından veya ses bağlantılarından yalnız sesi çıkar",
                    "Audio — extract audio only from video pages or audio links",
                ),
                ID_WIZARD_AUDIO,
            );
            set_check(f.simple_card, f.simple);
            set_check(f.advanced_card, !f.simple);
            set_check(f.video, f.modes.video);
            set_check(f.file, f.modes.file);
            set_check(f.audio, f.modes.audio);
            f.start = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Başla", "Start"),
                ID_WIZARD_START,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
            EnableWindow(f.start, wizard_start_enabled(f) as BOOL);
            SetFocus(f.simple_card);
        }
        DialogKind::Add(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "URL bağlantıları (her satıra bir bağlantı):",
                    "URL links (one link per line):",
                ),
                0,
            );
            f.urls = dialog_edit(dlg.hwnd, dlg.font, D_URLS, true, false);
            set_text(f.urls, &f.initial);
            SendMessageW(f.urls, EM_SETLIMITTEXT, 1_048_576, 0);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Hedef klasör:", "Target folder:"),
                0,
            );
            f.dir = dialog_edit(dlg.hwnd, dlg.font, D_DIR, false, false);
            set_text(
                f.dir,
                &dlg.app.snapshot().settings.download_dir.to_string_lossy(),
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Gözat...", "Browse..."),
                D_BROWSE,
                false,
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Dosya adı (isteğe bağlı, tek URL için):",
                    "File name (optional, for a single URL):",
                ),
                0,
            );
            f.filename = dialog_edit(dlg.hwnd, dlg.font, D_FILENAME, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İndirme türü:", "Download type:"),
                0,
            );
            f.kind = dialog_combo(dlg.hwnd, dlg.font, D_KIND);
            let modes = dlg.app.snapshot().settings.usage_modes;
            f.kind_values.clear();
            if modes.allows(DownloadKind::Auto) {
                combo_add(
                    f.kind,
                    crate::i18n::ui("Otomatik algıla", "Detect automatically"),
                );
                f.kind_values.push(DownloadKind::Auto);
            }
            if modes.file {
                combo_add(f.kind, crate::i18n::ui("Dosya", "File"));
                f.kind_values.push(DownloadKind::File);
            }
            if modes.video {
                combo_add(f.kind, "Video");
                f.kind_values.push(DownloadKind::Video);
            }
            if modes.audio {
                combo_add(f.kind, crate::i18n::ui("Yalnız ses", "Audio only"));
                f.kind_values.push(DownloadKind::Audio);
            }
            combo_select(f.kind, 0);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Bağlantı sayısı (boş = tür ayarı):",
                    "Connection count (empty = type default):",
                ),
                0,
            );
            f.connections = dialog_edit(dlg.hwnd, dlg.font, D_CONNECTIONS, false, true);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "SHA-256 (isteğe bağlı, 64 onaltılık karakter):",
                    "SHA-256 (optional, 64 hexadecimal characters):",
                ),
                0,
            );
            f.checksum = dialog_edit(dlg.hwnd, dlg.font, D_CHECKSUM, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "En erken başlama (yerel saat, YYYY-AA-GG SS:DD):",
                    "Earliest start (local time, YYYY-MM-DD HH:MM):",
                ),
                0,
            );
            f.schedule = dialog_edit(dlg.hwnd, dlg.font, D_SCHEDULE, false, false);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Devam / İndir", "Continue / Download"),
                D_ADD_OK,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Gelişmiş seçenekler", "Advanced options"),
                D_ADVANCED,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
            if f.analyze_first && !f.initial.trim().is_empty() {
                PostMessageW(dlg.hwnd, WM_COMMAND, D_ADD_ANALYZE as usize, 0);
            }
        }
        DialogKind::Settings(f) => {
            let tabs = control(
                dlg.hwnd,
                "SysTabControl32",
                "",
                // The strip paints itself so the dialog keeps one surface in
                // both themes instead of a themed page inside a plain dialog.
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | TCS_OWNERDRAWFIXED,
                0,
                S_TABS,
            );
            apply_font(tabs, dlg.font);
            tab_item(tabs, 0, crate::i18n::ui("Genel", "General"));
            // The simpler surface has one page: an advanced tab that only holds
            // rows a first-time user did not ask for is not offered at all.
            let simple = UiMode::parse(&f.value.ui_mode).is_simple();
            if !simple {
                tab_item(tabs, 1, crate::i18n::ui("Gelişmiş", "Advanced"));
                tab_item(tabs, 2, crate::i18n::ui("Ağ", "Network"));
                tab_item(tabs, 3, crate::i18n::ui("Davranış", "Behaviour"));
            }
            subclass(tabs, tabs_proc);

            // Genel: everyday choices, created first so layout_dialog can index
            // the label statics per tab.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Varsayılan indirme klasörü:", "Default download folder:"),
                0,
            );
            f.dir = dialog_edit(dlg.hwnd, dlg.font, S_DIR, false, false);
            set_text(f.dir, &f.value.download_dir.to_string_lossy());
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Gözat...", "Browse..."),
                S_BROWSE,
                false,
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kullanım amacı:", "Usage mode:"),
                0,
            );
            f.mode_video = dialog_check(dlg.hwnd, dlg.font, "Video", ID_SETTINGS_MODE_VIDEO);
            f.mode_file = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Dosya", "File"),
                ID_SETTINGS_MODE_FILE,
            );
            f.mode_audio = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Ses", "Audio"),
                ID_SETTINGS_MODE_AUDIO,
            );
            set_check(f.mode_video, f.value.usage_modes.video);
            set_check(f.mode_file, f.value.usage_modes.file);
            set_check(f.mode_audio, f.value.usage_modes.audio);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Sihirbazı yeniden aç...", "Reopen the wizard..."),
                ID_SETTINGS_REOPEN_WIZARD,
                false,
            );
            f.clipboard = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Panodaki yeni URL'leri algıla",
                    "Detect new URLs from the clipboard",
                ),
                S_CLIPBOARD,
            );
            set_check(f.clipboard, f.value.clipboard_watch);
            f.tray = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Pencere kapatıldığında bildirim alanına küçült",
                    "Minimize to the notification area when closing the window",
                ),
                S_TRAY,
            );
            set_check(f.tray, f.value.close_to_tray);
            f.startup = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Windows oturum açılışında başlat",
                    "Start at Windows sign-in",
                ),
                S_STARTUP,
            );
            set_check(f.startup, f.value.start_with_windows);
            f.notify = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İndirme tamamlanınca bildirim göster",
                    "Show a notification when a download completes",
                ),
                S_NOTIFY,
            );
            set_check(f.notify, f.value.notify_completion);
            f.dark = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Koyu pencere başlığı ve denetim teması",
                    "Dark window title and control theme",
                ),
                S_DARK,
            );
            set_check(f.dark, f.value.dark_mode);
            f.schedule = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Günlük çalışma saatlerini sınırla",
                    "Limit daily working hours",
                ),
                S_SCHEDULE,
            );
            set_check(f.schedule, f.value.schedule_enabled);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Başlangıç (SS:DD):", "Start (HH:MM):"),
                0,
            );
            f.start = dialog_edit(dlg.hwnd, dlg.font, S_START, false, false);
            set_text(f.start, &f.value.schedule_start);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Bitiş (SS:DD):", "End (HH:MM):"),
                0,
            );
            f.end = dialog_edit(dlg.hwnd, dlg.font, S_END, false, false);
            set_text(f.end, &f.value.schedule_end);
            f.update_check = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Başlangıçta güncellemeleri denetle (günde en fazla bir kez)",
                    "Check for updates at startup (at most once a day)",
                ),
                S_UPDATE_CHECK,
            );
            set_check(f.update_check, f.value.update_check_enabled);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Güncellemeleri şimdi denetle...",
                    "Check for updates now...",
                ),
                ID_SETTINGS_CHECK_UPDATES,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Eklentiyi tarayıcılara kur...",
                    "Install the extension in browsers...",
                ),
                ID_SETTINGS_INSTALL_EXTENSION,
                false,
            );

            // Gelişmiş: transfer knobs and logging.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Aynı anda etkin iş (1–16):",
                    "Concurrent active jobs (1–16):",
                ),
                0,
            );
            f.max_active = dialog_edit(dlg.hwnd, dlg.font, S_MAX_ACTIVE, false, true);
            set_text(f.max_active, &f.value.max_active.to_string());
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Dosya aralığı bağlantısı (1–16):",
                    "Connections per file (1–16):",
                ),
                0,
            );
            f.connections = dialog_edit(dlg.hwnd, dlg.font, S_CONNECTIONS, false, true);
            set_text(f.connections, &f.value.connections.to_string());
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Medya parçası eşzamanlılığı (1–16):",
                    "Media fragment concurrency (1–16):",
                ),
                0,
            );
            f.media_fragments = dialog_edit(dlg.hwnd, dlg.font, S_MEDIA_FRAGMENTS, false, true);
            set_text(
                f.media_fragments,
                &f.value.media_fragment_connections.to_string(),
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Sunucu başına iş ve bağlantı (1–16):",
                    "Jobs and connections per server (1–16):",
                ),
                0,
            );
            f.per_host = dialog_edit(dlg.hwnd, dlg.font, S_PER_HOST, false, true);
            set_text(f.per_host, &f.value.per_host_limit.to_string());
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Genel hız sınırı (KiB/sn, 0 = sınırsız):",
                    "Overall speed limit (KiB/s, 0 = unlimited):",
                ),
                0,
            );
            f.speed = dialog_edit(dlg.hwnd, dlg.font, S_SPEED, false, true);
            set_text(f.speed, &f.value.speed_limit_kib.to_string());
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Yeniden deneme sayısı (0–20):", "Retry count (0–20):"),
                0,
            );
            f.retry = dialog_edit(dlg.hwnd, dlg.font, S_RETRY, false, true);
            set_text(f.retry, &f.value.retry_limit.to_string());
            f.browser_transfer = dialog_check_wrapped(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Deneysel tarayıcı aktarımını etkinleştir — büyük dosyalarda bellek ve bütünlük sınırları geçerlidir",
                    "Enable experimental browser transfer — memory and integrity limits apply to large files",
                ),
                ID_SETTINGS_BROWSER_TRANSFER,
            );
            set_check(f.browser_transfer, f.value.experimental_browser_transfer);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Olay günlüğü:", "Event log:"),
                0,
            );
            f.log_level = dialog_combo(dlg.hwnd, dlg.font, S_LOG_LEVEL);
            for label in [
                crate::i18n::ui("Kapalı", "Off"),
                crate::i18n::ui("Yalnız hatalar", "Errors only"),
                crate::i18n::ui("Normal", "Normal"),
                crate::i18n::ui("Ayrıntılı", "Detailed"),
            ] {
                combo_add(f.log_level, label);
            }
            let level = Level::parse(&f.value.logging_level).unwrap_or(Level::Detailed);
            combo_select(
                f.log_level,
                match level {
                    Level::Off => 0,
                    Level::Error => 1,
                    Level::Normal => 2,
                    Level::Detailed => 3,
                },
            );
            f.log_hosts = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Site adlarını günlüğe kaydet", "Log site names"),
                S_LOG_HOSTS,
            );
            set_check(f.log_hosts, f.value.logging_hosts);
            // Genel, continued: the surface depth of the whole application, and
            // whether a site's own download entries are offered in the browser.
            // Both stay on the everyday page, because both are everyday
            // choices; the mode selector is how the dialog gets back to the
            // simple rows after it grew.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Arayüz düzeyi:", "Interface level:"),
                0,
            );
            f.ui_mode = dialog_combo(dlg.hwnd, dlg.font, S_UI_MODE);
            combo_add(f.ui_mode, crate::i18n::ui("Basit", "Basic"));
            combo_add(f.ui_mode, crate::i18n::ui("Gelişmiş", "Advanced"));
            combo_select(f.ui_mode, if simple { 0 } else { 1 });
            f.site_entries = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Site indirme girişlerini tarayıcıda göster",
                    "Show site download entries in the browser",
                ),
                S_SITE_ENTRIES,
            );
            set_check(f.site_entries, f.value.site_entries);
            // Arayüz dili: the switch itself. It rides the mode row because the
            // page has no row to spare.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Arayüz dili:", "Interface language:"),
                0,
            );
            f.ui_language = dialog_combo(dlg.hwnd, dlg.font, S_UI_LANGUAGE);
            combo_add(f.ui_language, "Türkçe");
            combo_add(f.ui_language, "English");
            combo_select(
                f.ui_language,
                if f.value.ui_language.trim().eq_ignore_ascii_case("en") {
                    1
                } else {
                    0
                },
            );
            // Labels of the two transfer rows the simple page shows. They are
            // separate statics so the advanced page keeps its own wording for
            // the same two fields instead of one label serving two surfaces.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Eşzamanlı indirme (1–16):", "Concurrent downloads (1–16):"),
                0,
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Bağlantı sayısı (1–16):", "Connection count (1–16):"),
                0,
            );
            // Ağ: proxy, the scheduled speed limit and the behaviour switches of a
            // running download. Labels 15..=21 in creation order.
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Proxy:", "Proxy:"), 0);
            f.proxy_mode = dialog_combo(dlg.hwnd, dlg.font, S_PROXY_MODE);
            combo_add(
                f.proxy_mode,
                crate::i18n::ui("Windows ayarı", "Windows setting"),
            );
            combo_add(f.proxy_mode, crate::i18n::ui("Proxy yok", "No proxy"));
            combo_add(f.proxy_mode, crate::i18n::ui("Elle", "Manual"));
            combo_select(
                f.proxy_mode,
                match f.value.proxy.mode.as_str() {
                    "none" => 1,
                    "manual" => 2,
                    _ => 0,
                },
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Proxy adresi:", "Proxy address:"),
                0,
            );
            f.proxy_url = dialog_edit(dlg.hwnd, dlg.font, S_PROXY_URL, false, false);
            set_text(f.proxy_url, &f.value.proxy.url);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Proxy kullanıcı adı:", "Proxy user name:"),
                0,
            );
            f.proxy_user = dialog_edit(dlg.hwnd, dlg.font, S_PROXY_USER, false, false);
            set_text(f.proxy_user, &f.value.proxy.username);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Proxy parolası (boş = değiştirme):",
                    "Proxy password (empty = keep):",
                ),
                0,
            );
            f.proxy_pass = control(
                dlg.hwnd,
                "EDIT",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | ES_AUTOHSCROLL as u32
                    | ES_PASSWORD as u32,
                WS_EX_CLIENTEDGE,
                S_PROXY_PASS,
            );
            apply_font(f.proxy_pass, dlg.font);
            f.speed_schedule = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Saatli hız sınırı", "Scheduled speed limit"),
                S_SPEED_SCHEDULE,
            );
            set_check(f.speed_schedule, f.value.speed_schedule_enabled);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Başlangıç:", "Start:"),
                0,
            );
            f.speed_start = dialog_edit(dlg.hwnd, dlg.font, S_SPEED_START, false, false);
            set_text(f.speed_start, &f.value.speed_schedule_start);
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Bitiş:", "End:"), 0);
            f.speed_end = dialog_edit(dlg.hwnd, dlg.font, S_SPEED_END, false, false);
            set_text(f.speed_end, &f.value.speed_schedule_end);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Saatli sınır (KiB/sn):", "Scheduled limit (KiB/s):"),
                0,
            );
            f.speed_kib = dialog_edit(dlg.hwnd, dlg.font, S_SPEED_KIB, false, true);
            set_text(f.speed_kib, &f.value.speed_schedule_kib.to_string());
            f.keep_awake = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İndirme sürerken bilgisayarın uykuya geçmesini engelle",
                    "Keep the computer awake while downloads run",
                ),
                S_KEEP_AWAKE,
            );
            set_check(f.keep_awake, f.value.keep_awake);
            f.sound = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İndirme tamamlanınca ses çal",
                    "Play a sound when a download completes",
                ),
                S_SOUND,
            );
            set_check(f.sound, f.value.completion_sound);
            f.takeover = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Tarayıcı indirmelerini devral (dosya türlerine göre)",
                    "Take over browser downloads (by file type)",
                ),
                S_TAKEOVER,
            );
            set_check(f.takeover, f.value.browser_takeover);
            // Davranış: labels 22 and 23 in creation order.
            f.completion_card = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İndirme bitince sağ altta açma kartı göster",
                    "Show an open card at the bottom right when a download completes",
                ),
                S_CARD,
            );
            set_check(f.completion_card, f.value.completion_card);
            f.metered = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Ölçülü (tarifeli) bağlantıda indirmeleri beklet",
                    "Hold downloads on a metered connection",
                ),
                S_METERED,
            );
            set_check(f.metered, f.value.pause_on_metered);
            f.hotkey = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Ctrl+Shift+D ile panodaki bağlantıyı her yerden ekle",
                    "Add the clipboard's link from anywhere with Ctrl+Shift+D",
                ),
                S_HOTKEY,
            );
            set_check(f.hotkey, f.value.global_hotkey);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Tamamlanana çift tıklama:",
                    "Double-click on a completed row:",
                ),
                0,
            );
            f.double_click = dialog_combo(dlg.hwnd, dlg.font, S_DOUBLE_CLICK);
            combo_add(
                f.double_click,
                crate::i18n::ui("Dosyayı aç", "Open the file"),
            );
            combo_add(
                f.double_click,
                crate::i18n::ui("Klasörde göster", "Show in folder"),
            );
            combo_select(
                f.double_click,
                if f.value.double_click == "folder" {
                    1
                } else {
                    0
                },
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Tamamlananları listeden kaldır (gün, 0 = hiç):",
                    "Remove completed from the list (days, 0 = never):",
                ),
                0,
            );
            f.history = dialog_edit(dlg.hwnd, dlg.font, S_HISTORY, false, true);
            set_text(f.history, &f.value.history_days.to_string());
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kaydet", "Save"),
                S_OK,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
        }
        DialogKind::Tools(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Video/ses indirmeleri için yt-dlp, FFmpeg ve Deno durumları:",
                    "yt-dlp, FFmpeg and Deno status for video/audio downloads:",
                ),
                0,
            );
            f.status = control(
                dlg.hwnd,
                "EDIT",
                crate::i18n::ui("Durum okunuyor...", "Reading status..."),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32
                    | WS_VSCROLL,
                WS_EX_CLIENTEDGE,
                T_STATUS,
            );
            apply_font(f.status, dlg.font);
            f.progress = control(
                dlg.hwnd,
                "msctls_progress32",
                "",
                WS_CHILD | WS_VISIBLE,
                0,
                T_PROGRESS,
            );
            SendMessageW(f.progress, PBM_SETRANGE32, 0, 100);
            f.install = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Eksikleri kur", "Install missing"),
                T_INSTALL,
                true,
            );
            f.update = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Tümünü güncelle", "Update all"),
                T_UPDATE,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Tarayıcı bağlantısı...", "Browser connection..."),
                T_BROWSER,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
            SetTimer(dlg.hwnd, TIMER_DIALOG, 300, None);
        }
        DialogKind::Queues(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kuyruklar:", "Queues:"),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, Q_LIST, false);
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Ad:", "Name:"), 0);
            f.name = dialog_edit(dlg.hwnd, dlg.font, Q_NAME, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Eşzamanlılık (1–16):", "Concurrency (1–16):"),
                0,
            );
            f.concurrency = dialog_edit(dlg.hwnd, dlg.font, Q_CONCURRENCY, false, true);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Bitiş eylemi:", "Completion action:"),
                0,
            );
            f.completion = dialog_combo(dlg.hwnd, dlg.font, Q_COMPLETION);
            for label in [
                crate::i18n::ui("Hiçbir şey", "Do nothing"),
                crate::i18n::ui("Bildirim", "Notification"),
                crate::i18n::ui("Bilgisayarı kapat", "Shut down the computer"),
                crate::i18n::ui("Seçili programı çalıştır", "Run the selected program"),
            ] {
                combo_add(f.completion, label);
            }
            f.enabled = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kuyruk etkin", "Queue enabled"),
                Q_ENABLED,
            );
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Zaman pencereleri: satır başına günler SS:DD-SS:DD · Pzt=0, Paz=6 · boş: her zaman",
                    "Time windows: days per line HH:MM-HH:MM · Mon=0, Sun=6 · empty: always",
                ),
                0,
            );
            f.windows = dialog_edit(dlg.hwnd, dlg.font, Q_WINDOWS, true, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Dönem kotası (bayt; 0: sınırsız):",
                    "Quota (bytes; 0: unlimited):",
                ),
                0,
            );
            f.quota = dialog_edit(dlg.hwnd, dlg.font, Q_QUOTA, false, true);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Programın tam yolu (yalnız program eylemi):",
                    "Full program path (program action only):",
                ),
                0,
            );
            f.program = dialog_edit(dlg.hwnd, dlg.font, Q_PROGRAM, false, false);
            // The caption column is 300 px wide; the long form used to wrap and clip
            // into the row below. The edit itself takes a JSON array.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Program bağımsız değişkenleri (JSON)",
                    "Program arguments (JSON)",
                ),
                0,
            );
            f.arguments = dialog_edit(dlg.hwnd, dlg.font, Q_ARGUMENTS, true, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İptal edilebilir geri sayım (saniye):",
                    "Cancellable countdown (seconds):",
                ),
                0,
            );
            f.delay = dialog_edit(dlg.hwnd, dlg.font, Q_DELAY, false, true);
            f.countdown = dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Etkin bitiş geri sayımı yok.",
                    "No active completion countdown.",
                ),
                Q_COUNTDOWN,
            );
            for (id, label) in [
                (Q_CREATE, crate::i18n::ui("Oluştur", "Create")),
                (Q_RENAME, crate::i18n::ui("Ad değiştir", "Rename")),
                (Q_DELETE, crate::i18n::ui("Sil", "Delete")),
                (
                    Q_UPDATE,
                    crate::i18n::ui("Politikayı kaydet", "Save policy"),
                ),
                (
                    Q_MOVE_JOB,
                    crate::i18n::ui("Seçili işi taşı", "Move selected job"),
                ),
                (
                    Q_CANCEL_COMPLETION,
                    crate::i18n::ui("Geri sayımı iptal", "Cancel countdown"),
                ),
            ] {
                dialog_button(dlg.hwnd, dlg.font, label, id, false);
            }
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
            if let Err(error) = refresh_queue_controls(dlg.hwnd, f, &dlg.app.snapshot()) {
                crate::logging::record(
                    crate::logging::Event::warn("gui.queue_controls")
                        .detail(format!("Kuyruk denetimleri yüklenemedi: {error:#}")),
                );
            }
            SetTimer(dlg.hwnd, TIMER_DIALOG, 300, None);
        }
        DialogKind::Rules(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Kurallar (seçili satır düzenlenir; seçim yoksa yeni eklenir):",
                    "Rules (edit the selected row; with no selection a new rule is added):",
                ),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, R_LIST, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Tam host:", "Exact host:"),
                0,
            );
            f.host = dialog_edit(dlg.hwnd, dlg.font, R_HOST, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Hedef klasör:", "Target folder:"),
                0,
            );
            f.dir = dialog_edit(dlg.hwnd, dlg.font, R_DIR, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Öncelik:", "Priority:"),
                0,
            );
            f.priority = dialog_edit(dlg.hwnd, dlg.font, R_PRIORITY, false, true);
            set_text(f.priority, "0");
            f.subdomains = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Alt hostları dahil et", "Include subdomains"),
                R_SUBDOMAINS,
            );
            f.file = dialog_check(dlg.hwnd, dlg.font, crate::i18n::ui("Dosya", "File"), R_FILE);
            f.video = dialog_check(dlg.hwnd, dlg.font, "Video", R_VIDEO);
            f.audio = dialog_check(dlg.hwnd, dlg.font, crate::i18n::ui("Ses", "Audio"), R_AUDIO);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Yeni kural", "New rule"),
                R_NEW,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Ekle / güncelle", "Add / update"),
                R_SAVE,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Seçileni kaldır", "Remove selected"),
                R_REMOVE,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
            refresh_rule_list(f, &dlg.app.snapshot().settings.folder_rules);
        }
        DialogKind::Crawler(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Başlangıç URL'si:", "Starting URL:"),
                0,
            );
            f.url = dialog_edit(dlg.hwnd, dlg.font, C_URL, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Derinlik:", "Depth:"),
                0,
            );
            f.depth = dialog_edit(dlg.hwnd, dlg.font, C_DEPTH, false, true);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("En fazla sayfa:", "Maximum pages:"),
                0,
            );
            f.pages = dialog_edit(dlg.hwnd, dlg.font, C_PAGES, false, true);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("En fazla aday:", "Maximum candidates:"),
                0,
            );
            f.candidates = dialog_edit(dlg.hwnd, dlg.font, C_CANDIDATES, false, true);
            let policy = dlg.app.snapshot().settings.crawler_policy.clone();
            set_text(f.depth, &policy.max_depth.to_string());
            set_text(f.pages, &policy.max_pages.to_string());
            set_text(f.candidates, &policy.max_candidates.to_string());
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Sınırlarla tara", "Scan within limits"),
                C_SCAN,
                true,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, C_LIST, true);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Yalnız seçilen adayları ekle",
                    "Add only the selected candidates",
                ),
                C_ADD,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
        }
        DialogKind::Synchronization(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Eşitleme kayıtları:", "Sync policies:"),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, Y_LIST, false);
            dialog_label(dlg.hwnd, dlg.font, "URL:", 0);
            f.url = dialog_edit(dlg.hwnd, dlg.font, Y_URL, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Hedef dosya:", "Target file:"),
                0,
            );
            f.dir = dialog_edit(dlg.hwnd, dlg.font, Y_DIR, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kontrol aralığı (dakika):", "Check interval (minutes):"),
                0,
            );
            f.interval = dialog_edit(dlg.hwnd, dlg.font, Y_INTERVAL, false, true);
            set_text(f.interval, "60");
            f.enabled = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Periyodik kontrol etkin", "Periodic check enabled"),
                Y_ENABLED,
            );
            set_check(f.enabled, true);
            f.overwrite = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Eski dosyanın üzerine atomik yaz (varsayılan: sürüm koru)",
                    "Atomically overwrite the old file (default: keep version)",
                ),
                Y_OVERWRITE,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Yeni kayıt", "New record"),
                Y_NEW,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Ekle / güncelle", "Add / update"),
                Y_SAVE,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Seçileni kaldır", "Remove selected"),
                Y_REMOVE,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Şimdi kontrol et", "Check now"),
                Y_RUN,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
            refresh_sync_list(f, &dlg.app.snapshot().settings.synchronization_policies);
        }
        DialogKind::Events(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Son olaylar (en yeni altta):",
                    "Latest events (newest at bottom):",
                ),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, E_LIST, false);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Günlük klasörünü aç", "Open log folder"),
                E_OPEN,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Teşhis paketi oluştur", "Create a diagnostic package"),
                E_PACKAGE,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                E_CLOSE,
                false,
            );
            refresh_event_list(f);
        }
        DialogKind::Speed(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                &crate::i18n::ui_owned!(
                    format!(
                        "Seçili {} indirme için hız sınırı (KiB/sn, 0 = genel sınır):",
                        f.ids.len()
                    ),
                    format!(
                        "Speed limit for the {} selected download(s) (KiB/s, 0 = global limit):",
                        f.ids.len()
                    )
                ),
                0,
            );
            f.edit = dialog_edit(dlg.hwnd, dlg.font, SP_EDIT, false, true);
            set_text(f.edit, &f.initial.to_string());
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Uygula", "Apply"),
                SP_OK,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
            SetFocus(f.edit);
        }
        DialogKind::Rename(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Yeni dosya adı:", "New file name:"),
                0,
            );
            f.edit = dialog_edit(dlg.hwnd, dlg.font, RN_EDIT, false, false);
            set_text(f.edit, &f.initial);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Yeniden adlandır", "Rename"),
                RN_OK,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
            SetFocus(f.edit);
            // Select the stem, not the extension, like Explorer does.
            let stem = f.initial.rfind('.').unwrap_or(f.initial.len());
            let stem = f.initial[..stem].encode_utf16().count();
            SendMessageW(f.edit, EM_SETSEL, 0, stem as isize);
        }
        DialogKind::Logins(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Dosya indirmelerinde yalnız bu sunuculara gönderilen HTTP Basic girişleri:",
                    "HTTP Basic logins sent to exactly these hosts for file downloads:",
                ),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, L_LIST, false);
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Sunucu:", "Host:"), 0);
            f.host = dialog_edit(dlg.hwnd, dlg.font, L_HOST, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kullanıcı adı:", "User name:"),
                0,
            );
            f.user = dialog_edit(dlg.hwnd, dlg.font, L_USER, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Parola (boş = değiştirme):", "Password (empty = keep):"),
                0,
            );
            f.pass = control(
                dlg.hwnd,
                "EDIT",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | ES_AUTOHSCROLL as u32
                    | ES_PASSWORD as u32,
                WS_EX_CLIENTEDGE,
                L_PASS,
            );
            apply_font(f.pass, dlg.font);
            f.allow_http = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Şifresiz HTTP bağlantısında da gönder",
                    "Also send over unencrypted HTTP",
                ),
                L_ALLOW_HTTP,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Ekle / güncelle", "Add / update"),
                L_ADD,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kaldır", "Remove"),
                L_REMOVE,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kaydet", "Save"),
                L_SAVE,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                D_CANCEL,
                false,
            );
            refresh_login_list(f);
        }
        DialogKind::JobLog(f) => {
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Bu indirmenin kayıtları (en yeni altta):",
                    "Logs for this download (newest at bottom):",
                ),
                0,
            );
            f.list = dialog_list(dlg.hwnd, dlg.font, J_LIST, false);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kopyala", "Copy"),
                J_COPY,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Günlük klasörünü aç", "Open log folder"),
                J_OPEN,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                J_CLOSE,
                false,
            );
            refresh_job_log(f);
            SetTimer(dlg.hwnd, TIMER_DIALOG, 300, None);
        }
        DialogKind::Media(f) => {
            f.summary = control(
                dlg.hwnd,
                "EDIT",
                crate::i18n::ui("Çözünürlükler hazırlanıyor...", "Preparing resolutions..."),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_BORDER
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32,
                WS_EX_CLIENTEDGE,
                M_SUMMARY,
            );
            apply_font(f.summary, dlg.font);
            // The popup this replaces let the user edit the saved name, so the
            // picker keeps an editable field next to the source name.
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Dosya adı:", "File name:"),
                0,
            );
            f.name = dialog_edit(dlg.hwnd, dlg.font, M_NAME, false, false);
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Çıktı:", "Output:"), 0);
            f.kind = dialog_combo(dlg.hwnd, dlg.font, M_KIND);
            let modes = dlg.app.snapshot().settings.usage_modes;
            if modes.video {
                combo_add(f.kind, "MP4 video");
                f.kind_values
                    .push((DownloadKind::Video, Some("mp4".into())));
                combo_add(f.kind, "MKV video");
                f.kind_values
                    .push((DownloadKind::Video, Some("mkv".into())));
            }
            if modes.audio {
                combo_add(f.kind, crate::i18n::ui("Yalnız ses", "Audio only"));
                f.kind_values.push((DownloadKind::Audio, None));
            }
            let preferred = f
                .kind_values
                .iter()
                .position(|(kind, container)| {
                    (*kind == DownloadKind::Audio && f.base.kind == DownloadKind::Audio)
                        || (*kind == DownloadKind::Video
                            && f.base.kind != DownloadKind::Audio
                            && container.as_deref()
                                == Some(f.base.container.as_deref().unwrap_or("mp4")))
                })
                .unwrap_or(0);
            combo_select(f.kind, preferred as i32);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Kalite (tam biçim; codec/FPS/HDR ayrıntıları listelenir):",
                    "Quality (full format; codec/FPS/HDR details listed):",
                ),
                0,
            );
            f.format = dialog_combo(dlg.hwnd, dlg.font, M_FORMAT);
            combo_add(
                f.format,
                crate::i18n::ui("En iyi kalite (otomatik)", "Best quality (automatic)"),
            );
            combo_select(f.format, 0);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Ses dönüştürme biçimi:", "Audio conversion format:"),
                0,
            );
            f.audio = dialog_combo(dlg.hwnd, dlg.font, M_AUDIO);
            for a in [
                crate::i18n::ui("Özgün ses", "Original audio"),
                "mp3",
                "m4a",
                "opus",
                "flac",
            ] {
                combo_add(f.audio, a);
            }
            combo_select(f.audio, 0);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Ses parçaları (Ctrl/Shift; boş = özgün):",
                    "Audio tracks (Ctrl/Shift; empty = original):",
                ),
                0,
            );
            f.audio_tracks = control(
                dlg.hwnd,
                "LISTBOX",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | LBS_EXTENDEDSEL as u32
                    | LBS_NOINTEGRALHEIGHT as u32,
                WS_EX_CLIENTEDGE,
                ID_MEDIA_AUDIO_TRACKS,
            );
            apply_font(f.audio_tracks, dlg.font);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Yerleşik altyazılar (Ctrl/Shift, çoklu):",
                    "Embedded subtitles (Ctrl/Shift, multiple):",
                ),
                0,
            );
            f.subtitles = control(
                dlg.hwnd,
                "LISTBOX",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | LBS_EXTENDEDSEL as u32
                    | LBS_NOINTEGRALHEIGHT as u32,
                WS_EX_CLIENTEDGE,
                ID_MEDIA_SUBTITLE_TRACKS,
            );
            apply_font(f.subtitles, dlg.font);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Harici altyazı URL'si (VTT/SRT):",
                    "External subtitle URL (VTT/SRT):",
                ),
                0,
            );
            f.external_url = dialog_edit(dlg.hwnd, dlg.font, ID_MEDIA_EXTERNAL_URL, false, false);
            dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Dil kodu:", "Language code:"),
                0,
            );
            f.external_language =
                dialog_edit(dlg.hwnd, dlg.font, ID_MEDIA_EXTERNAL_LANGUAGE, false, false);
            dialog_label(dlg.hwnd, dlg.font, crate::i18n::ui("Etiket:", "Label:"), 0);
            f.external_label =
                dialog_edit(dlg.hwnd, dlg.font, ID_MEDIA_EXTERNAL_LABEL, false, false);
            f.external_kind = dialog_combo(dlg.hwnd, dlg.font, ID_MEDIA_EXTERNAL_KIND);
            combo_add(f.external_kind, "VTT");
            combo_add(f.external_kind, "SRT");
            combo_select(f.external_kind, 0);
            f.external_default = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Varsayılan", "Default"),
                ID_MEDIA_EXTERNAL_DEFAULT,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Altyazıyı ekle", "Add subtitle"),
                ID_MEDIA_EXTERNAL_ADD,
                false,
            );
            f.external_list = control(
                dlg.hwnd,
                "LISTBOX",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | LBS_NOINTEGRALHEIGHT as u32,
                WS_EX_CLIENTEDGE,
                ID_MEDIA_EXTERNAL_LIST,
            );
            apply_font(f.external_list, dlg.font);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Seçileni kaldır", "Remove selected"),
                ID_MEDIA_EXTERNAL_REMOVE,
                false,
            );
            f.full_verification = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "İndirme sonunda tam içerik doğrulaması yap",
                    "Verify full content after the download finishes",
                ),
                ID_MEDIA_FULL_VERIFICATION,
            );
            set_check(f.full_verification, f.base.full_verification);
            f.playlist = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Playlist'i indir", "Download the playlist"),
                M_PLAYLIST,
            );
            set_check(f.playlist, f.base.playlist);
            // Renewal binds an awaiting-source job to this exact handoff
            // (BeginSourceRefresh + CompleteSourceRefresh); it never queues a
            // second download for the same selection. The row is compact: the
            // combo names the pending jobs it can rebind.
            f.renew_job = dialog_combo(dlg.hwnd, dlg.font, M_RENEW_JOB);
            combo_add(
                f.renew_job,
                crate::i18n::ui("Bekleyen iş yok", "No pending job"),
            );
            f.renew_restart = dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İndirmeyi yeniden başlat", "Restart the download"),
                M_RENEW_RESTART,
            );
            set_check(f.renew_restart, true);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kaynağı yenile", "Refresh source"),
                M_RENEW,
                false,
            );
            EnableWindow(GetDlgItem(dlg.hwnd, M_RENEW), 0);
            EnableWindow(f.renew_job, 0);
            EnableWindow(f.renew_restart, 0);
            refresh_external_subtitle_list(f);
            f.playlist_items = dialog_edit(dlg.hwnd, dlg.font, M_PLAYLIST_ITEMS, false, false);
            let cue = wide(crate::i18n::ui("öğeler: 1-10,15", "items: 1-10,15"));
            SendMessageW(f.playlist_items, EM_SETCUEBANNER, 1, cue.as_ptr() as LPARAM);
            if let Some(items) = &f.base.playlist_items {
                set_text(f.playlist_items, items);
            }
            f.remember = dialog_check(
                dlg.hwnd,
                dlg.font,
                &crate::i18n::ui_owned!(
                    format!(
                        "{} için bu seçimi hatırla ve bir daha sorma",
                        if f.host.is_empty() {
                            "Bu site"
                        } else {
                            f.host.as_str()
                        }
                    ),
                    format!(
                        "Remember this choice for {} and don't ask again",
                        if f.host.is_empty() {
                            "this site"
                        } else {
                            f.host.as_str()
                        }
                    )
                ),
                M_REMEMBER,
            );
            set_check(f.remember, f.auto_queue);
            f.thumbnail = control(
                dlg.hwnd,
                "STATIC",
                "",
                // SS_BITMAP | SS_CENTERIMAGE.
                WS_CHILD | 0x000E | 0x0200,
                0,
                M_THUMBNAIL,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("MP3 olarak indir", "Download as MP3"),
                M_MP3,
                false,
            );
            EnableWindow(GetDlgItem(dlg.hwnd, M_MP3), 0);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İndir", "Download"),
                M_QUEUE,
                true,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Gelişmiş seçenekler", "Advanced options"),
                D_ADVANCED,
                false,
            );
            // Only a failed analysis has anything to report, so the button is
            // created hidden and the picker's own timer shows it together with
            // the error line it belongs to.
            let report = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Hata bildir", "Report problem"),
                M_REPORT,
                false,
            );
            ShowWindow(report, SW_HIDE);
            f.video_formats.clear();
            EnableWindow(GetDlgItem(dlg.hwnd, M_QUEUE), 0);
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal", "Cancel"),
                D_CANCEL,
                false,
            );
            SetTimer(dlg.hwnd, TIMER_DIALOG, 300, None);
        }
        DialogKind::Progress(f) => {
            f.summary = dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İndirme hazırlanıyor...", "Preparing the download..."),
                P_SUMMARY,
            );
            f.hint = dialog_label(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui(
                    "Kapatmak indirmeyi durdurmaz; iptal için İptal et.",
                    "Closing does not stop the download; use Cancel to abort it.",
                ),
                P_HINT,
            );
            // One row per job the handoff queued: a playlist is worked one item
            // at a time, and the detail below always describes the row that is
            // selected rather than an arbitrary first one. The rows report the
            // user's own selection back, which is why they carry LBS_NOTIFY.
            f.list = control(
                dlg.hwnd,
                "LISTBOX",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | LBS_NOTIFY as u32
                    | LBS_NOINTEGRALHEIGHT as u32,
                WS_EX_CLIENTEDGE,
                P_LIST,
            );
            apply_font(f.list, dlg.font);
            f.status = control(
                dlg.hwnd,
                "EDIT",
                "",
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_BORDER
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32
                    | WS_VSCROLL,
                WS_EX_CLIENTEDGE,
                P_STATUS,
            );
            apply_font(f.status, dlg.font);
            f.progress = control(
                dlg.hwnd,
                "msctls_progress32",
                "",
                WS_CHILD | WS_VISIBLE,
                0,
                P_PROGRESS,
            );
            SendMessageW(f.progress, PBM_SETRANGE32, 0, 1000);
            f.action = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Duraklat", "Pause"),
                P_ACTION,
                true,
            );
            f.cancel = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("İptal et", "Cancel"),
                P_CANCEL,
                false,
            );
            f.open_file = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Dosyayı aç", "Open file"),
                P_OPEN_FILE,
                false,
            );
            f.open_folder = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Klasörde göster", "Show in folder"),
                P_OPEN_FOLDER,
                false,
            );
            dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Küçült", "Minimize"),
                P_MINI,
                false,
            );
            dialog_check(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Üstte tut", "Keep on top"),
                P_PIN,
            );
            f.close = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Kapat", "Close"),
                P_CLOSE,
                false,
            );
            // Only a failed job has anything to report: the button is created
            // hidden and the job view shows it with the failure itself.
            f.report = dialog_button(
                dlg.hwnd,
                dlg.font,
                crate::i18n::ui("Hata bildir", "Report problem"),
                P_REPORT,
                false,
            );
            ShowWindow(f.report, SW_HIDE);
            EnableWindow(f.open_file, 0);
            EnableWindow(f.open_folder, 0);
            SetTimer(dlg.hwnd, TIMER_DIALOG, 300, None);
        }
    }
}
