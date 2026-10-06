"""Generates the error-code outputs from the single authored table below.

Run `python scripts/generate-error-codes.py` after editing the table. It writes
`src/error_codes.rs` (registry and tests) and `browser/chromium/codes.js` (extension mirror);
the Rust test `the_extension_mirror_lists_exactly_the_same_codes` fails when the two drift
apart. A review table of every code lands in the ignored `target/error-codes-table.md`.
The emitted Rust already satisfies `cargo fmt --check`; keep it that way when editing the
templates here (`cargo fmt --check` runs in `scripts/check.ps1`).
"""

import subprocess
from pathlib import Path


def format_rust(path: Path) -> None:
    """Keeps the emitted registry identical to what `cargo fmt --check` expects."""
    try:
        subprocess.run(
            ["rustfmt", "--edition", "2021", str(path)],
            check=True,
            capture_output=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"rustfmt skipped ({error}); run `cargo fmt` before committing")

# code, area, stage, severity, action, retryable, message_tr, message_en, description_en

MACHINERY = """/// An error carrying a stable code. `anyhow` keeps it in the chain, so `code_of` finds it
/// even after `context` wrappers were added.
#[derive(Debug)]
pub struct CodedError {
    pub code: Code,
    message: String,
}

impl CodedError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CodedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.code, self.message)
    }
}

impl std::error::Error for CodedError {}

/// Builds a coded error whose display text already carries the code.
pub fn coded(code: Code, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CodedError::new(code, message))
}

/// Builds a coded error from the registry message.
pub fn coded_default(code: Code) -> anyhow::Error {
    coded(code, definition(code).map(|entry| crate::i18n::ui(entry.message_tr, entry.message_en)).unwrap_or_else(|| crate::i18n::ui("Bilinmeyen hata", "Unknown error")))
}

/// The code carried by an error chain, if any.
pub fn code_of(error: &anyhow::Error) -> Option<Code> {
    if let Some(coded) = error.downcast_ref::<CodedError>() {
        return Some(coded.code);
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(current) = source {
        if let Some(coded) = current.downcast_ref::<CodedError>() {
            return Some(coded.code);
        }
        source = current.source();
    }
    None
}

/// Who can resolve a failure. The log records this so a site-side block is not mistaken for an
/// application defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The application or its managed tools must change.
    App,
    /// The remote site refused the request, or no longer serves that media.
    Site,
    /// Rights management; unsupported by design and not fixable on this side.
    Drm,
    /// The player could not be observed from the extension (closed roots, worker-only, MSE).
    Observation,
}

impl Owner {
    pub const fn as_str(self) -> &'static str {
        match self {
            Owner::App => "app",
            Owner::Site => "site",
            Owner::Drm => "drm",
            Owner::Observation => "observation",
        }
    }
}

/// Classification of a code by who can resolve it. Only the exceptions are listed; every other
/// code names an application-side failure that this project can fix.
pub fn owner(code: Code) -> Owner {
    match code.0 {
        "SSD-MED-003" | "SSD-MED-004" => Owner::Drm,
        "SSD-MED-007" | "SSD-MED-008" | "SSD-EXT-010" => Owner::Site,
        "SSD-EXT-007" => Owner::Observation,
        _ => Owner::App,
    }
}

/// The code carried by the leading `SSD-XXX-NNN` token of a plain message.
///
/// Stored text (a job failure, a snapshot field) has already lost its error type, so the
/// registry is identified by its own syntax. Only known codes are accepted.
pub fn from_text(text: &str) -> Option<Code> {
    let token = text.split_whitespace().next()?.trim();
    let code = TABLE.iter().find(|entry| entry.code.0 == token)?.code;
    Some(code)
}

/// The code of an error chain, or of its redacted display text.
pub fn code_of_text(error: &anyhow::Error) -> Option<Code> {
    code_of(error).or_else(|| from_text(&format!("{error:#}")))
}

/// `code_of` plus the registry row, for callers that report both.
pub fn code_definition_of(error: &anyhow::Error) -> Option<(Code, &'static Definition)> {
    let code = code_of(error)?;
    Some((code, definition(code)?))
}

/// Attaches the registry message to a `bail!`-style site while keeping the code.
#[macro_export]
macro_rules! bail_code {
    ($code:expr) => {
        return Err($crate::error_codes::coded_default($code))
    };
    ($code:expr, $($arg:tt)*) => {
        return Err($crate::error_codes::coded($code, format!($($arg)*)))
    };
}
"""

CODES = [
    # --- bridge / native messaging -------------------------------------------------
    ("BRG-001", "bridge", "bridge", "error", "report", False,
     "Eklenti kimliği doğrulanamadı; istek reddedildi",
     "Extension identity could not be verified; request rejected",
     "A browser action arrived from an extension origin other than the registered one."),
    ("BRG-002", "bridge", "bridge", "error", "report", False,
     "Köprü mesajı geçersiz (çerçeve veya boyut)",
     "Invalid bridge message (frame or size)",
     "The native message frame was empty, truncated or larger than the protocol limit."),
    ("BRG-003", "bridge", "bridge", "error", "report", False,
     "Bilinmeyen köprü eylemi",
     "Unknown bridge action",
     "The action name is not part of the browser-facing action set."),
    ("BRG-004", "bridge", "bridge", "error", "report", False,
     "Bu eylem tarayıcı bağlamında kullanılamaz",
     "This action cannot be used from the browser context",
     "The action is destructive or credential-bearing and is not exposed to the browser."),
    ("BRG-005", "bridge", "bridge", "error", "open_app", True,
     "SSDownload uygulamasına ulaşılamadı",
     "Could not reach the SSDownload application",
     "The desktop process is not running or its local pipe is unavailable."),
    ("BRG-006", "bridge", "bridge", "error", "update", False,
     "Uygulama ve eklenti sürümleri uyuşmuyor",
     "App and extension versions do not match",
     "Capability protocol version or usage-mode shape differs between app and extension."),
    ("BRG-007", "bridge", "bridge", "info", "retry", True,
     "Aynı anda tek inceleme çalışabilir",
     "Only one inspection can run at a time",
     "An inspection was already running; the request was refused instead of queued."),
    # --- media resolution ----------------------------------------------------------
    ("MED-001", "media", "discovery", "error", "refresh", True,
     "İndirilebilir medya formatı bulunamadı",
     "No downloadable media format found",
     "The extractor reported no non-DRM downloadable format for the address."),
    ("MED-002", "media", "discovery", "error", "refresh", False,
     "Bu adres tek bir medya içermiyor (liste veya açılış sayfası olabilir). Videonun kendi sayfasını açıp indirmeyi oradan başlatın",
     "This address does not contain a single media item (it may be a playlist or landing page). Open the video's own page and start the download there",
     "The extractor returned a playlist with zero entries: the address is a feed, not one media item."),
    ("MED-003", "media", "discovery", "error", "unsupported", False,
     "Bu medya DRM korumalı; desteklenmiyor",
     "This media is DRM protected and not supported",
     "DRM protection was detected on the resolved media."),
    ("MED-004", "media", "discovery", "error", "unsupported", False,
     "Liste DRM korumalı bir öğe içeriyor",
     "The playlist contains a DRM protected entry",
     "A playlist entry is DRM protected and cannot be handled safely."),
    ("MED-005", "media", "discovery", "error", "report", False,
     "Adres geçersiz veya desteklenmiyor",
     "The address is invalid or unsupported",
     "The address could not be parsed or did not pass the supported-scheme check."),
    ("MED-006", "media", "discovery", "error", "retry", True,
     "Medya çözümleyici (extractor) hata verdi",
     "The media extractor failed",
     "The extractor child process failed; its verbatim error is attached to the log."),
    ("MED-007", "media", "discovery", "error", "open_page", True,
     "Site bot koruması veya erişim engeli bildirdi (403)",
     "The site reported bot protection or an access block (403)",
     "The origin answered 401/403 or a bot check; opening the page in the browser usually clears it."),
    ("MED-008", "media", "discovery", "error", "open_page", False,
     "Medya bölge kısıtlı, kaldırılmış veya oturum gerektiriyor",
     "The media is geo restricted, removed, or requires sign-in",
     "The extractor reported geo restriction, removal or a sign-in requirement."),
    ("MED-009", "media", "validation", "warn", "unsupported", False,
     "Canlı yayın desteği sınırlı",
     "Live stream support is limited",
     "The media is live; a live capture is outside the supported contract."),
    ("MED-010", "media", "discovery", "warn", "refresh", True,
     "Bu oynatıcının medya isteği yakalanamadı",
     "This player's media request could not be captured",
     "The player never produced an observable media request (MSE/blob); the page address is used instead."),
    ("MED-011", "media", "discovery", "error", "retry", True,
     "Medya analizi zaman aşımına uğradı",
     "Media analysis timed out",
     "The extractor did not answer within the analysis budget."),
    ("MED-012", "media", "audio", "error", "refresh", True,
     "Seçilen ses veya altyazı parçası bulunamadı",
     "The selected audio or subtitle track was not found",
     "A selected track disappeared between analysis and download."),
    ("MED-013", "media", "subtitle", "error", "retry", True,
     "Dış altyazı indirilemedi",
     "The external subtitle could not be downloaded",
     "An external subtitle request failed; embedded subtitles remain available."),
    ("MED-014", "media", "validation", "error", "settings", False,
     "Seçilen kalite veya kapsayıcı bu kaynakla uyumsuz",
     "The selected quality or container is incompatible with this source",
     "The requested container/quality cannot be produced from the available formats."),
    ("MED-015", "media", "manifest", "error", "refresh", True,
     "Seçilen kalite indirme anında kaynakta bulunamadı",
     "The selected quality is no longer available at the source",
     "The manifest no longer offers the analyzed quality when it is resolved again for the download."),
    ("MED-016", "media", "validation", "error", "report", False,
     "Seçilen ses dil etiketi çıktıya yazılamadı",
     "The selected audio language tag could not be written to the output",
     "The output container could not carry the selected audio language tag, so the marked language cannot be verified."),
    # --- tools ---------------------------------------------------------------------
    ("TOL-001", "tools", "source", "error", "settings", True,
     "Gerekli medya bileşenleri kurulu değil",
     "Required media components are not installed",
     "A managed tool is missing from the active tools version."),
    ("TOL-002", "tools", "source", "error", "retry", True,
     "Araç bütünlük doğrulaması başarısız",
     "Tool integrity verification failed",
     "The tool archive or executable did not match its recorded hash."),
    ("TOL-003", "tools", "source", "error", "retry", True,
     "Araç başlatılamadı",
     "The tool could not be started",
     "The tool process could not be spawned or its job assignment failed."),
    ("TOL-004", "tools", "source", "error", "update", False,
     "Araç sürümü desteklenmiyor",
     "The tool version is not supported",
     "The installed tool version is older than the supported minimum."),
    # --- network -------------------------------------------------------------------
    ("NET-001", "network", "source", "error", "retry", True,
     "Alan adı çözümlenemedi (DNS)",
     "Name resolution failed (DNS)",
     "Name resolution failed for the target host."),
    ("NET-002", "network", "source", "error", "retry", True,
     "TLS sertifika doğrulaması başarısız",
     "TLS certificate validation failed",
     "The TLS handshake or certificate validation failed."),
    ("NET-003", "network", "source", "error", "retry", True,
     "Proxy tüneli kurulamadı",
     "The proxy tunnel could not be established",
     "The proxy CONNECT tunnel was refused or reset."),
    ("NET-004", "network", "source", "error", "settings", True,
     "Sunucu yetkilendirme istedi (401/403)",
     "The server requested authorization (401/403)",
     "The origin requires credentials or a session cookie."),
    ("NET-005", "network", "source", "error", "refresh", False,
     "Kaynak bulunamadı (404)",
     "Resource not found (404)",
     "The origin answered 404 for the requested resource."),
    ("NET-006", "network", "source", "warn", "retry", True,
     "Sunucu meşgul (429/503)",
     "Server busy (429/503)",
     "The origin asked the client to slow down; the governor reduces concurrency."),
    ("NET-007", "network", "source", "error", "retry", True,
     "Bağlantı zaman aşımına uğradı",
     "The connection timed out",
     "A connect or transfer timeout expired."),
    ("NET-008", "network", "source", "error", "retry", True,
     "Bağlantı karşı tarafça sıfırlandı",
     "The connection was reset by the peer",
     "The peer aborted the connection mid-transfer."),
    ("NET-009", "network", "segment", "error", "retry", True,
     "Aralık (range) isteği kabul edilmedi",
     "The range request was not accepted",
     "The server refused the byte range used for segmented transfer."),
    ("NET-010", "network", "source", "warn", "settings", True,
     "Yerel hız sınırı etkin",
     "A local speed limit is in effect",
     "A configured speed limit is slowing the transfer down."),
    # --- transfer ------------------------------------------------------------------
    ("TRF-001", "transfer", "segment", "error", "retry", True,
     "Medya parçası indirilemedi",
     "A media segment could not be downloaded",
     "A fragment or segment request failed after its retries."),
    ("TRF-002", "transfer", "validation", "error", "retry", True,
     "Parçalar birleştirilemedi",
     "The parts could not be merged",
     "Muxing the downloaded parts failed."),
    ("TRF-003", "transfer", "validation", "error", "retry", True,
     "Boyut veya SHA-256 doğrulaması başarısız",
     "Size or SHA-256 verification failed",
     "The finished file did not match its expected size or digest."),
    ("TRF-004", "transfer", "output", "error", "retry", True,
     "Çıktı yayınlanamadı",
     "The output could not be published",
     "The final atomic publication step failed."),
    ("TRF-005", "transfer", "output", "error", "retry", True,
     "Hedef dosya kilitli",
     "The destination file is locked",
     "The destination is locked by another process."),
    ("TRF-006", "transfer", "output", "error", "settings", True,
     "Disk alanı yetersiz",
     "Not enough disk space",
     "The destination volume ran out of space during transfer."),
    ("TRF-007", "transfer", "output", "error", "settings", False,
     "Çıktı yolu geçersiz veya çok uzun",
     "The output path is invalid or too long",
     "The output path failed validation or exceeded the extended-length limit."),
    ("TRF-008", "transfer", "source", "info", "retry", True,
     "İş kullanıcı tarafından iptal edildi",
     "The job was cancelled by the user",
     "The job was cancelled; partial data is preserved."),
    ("TRF-009", "transfer", "source", "info", "retry", True,
     "İş duraklatıldı",
     "The job is paused",
     "The job is paused and resumable."),
    ("TRF-010", "transfer", "source", "info", "retry", True,
     "Aynı istek kimliği zaten kuyrukta",
     "The same request id is already queued",
     "An identical request id was already queued; no duplicate was created."),
    ("TRF-011", "transfer", "fragment", "error", "retry", True,
     "Parça atlandı; akış baştan indirilecek",
     "A fragment was skipped; the stream will be downloaded from the start",
     "A fragment was skipped during the download, so the partial stream is discarded instead of being resumed."),
    ("TRF-012", "transfer", "validation", "error", "retry", True,
     "Akış bütünlüğü doğrulanamadı",
     "Stream integrity could not be verified",
     "An audio or video stream is shorter or longer than its counterpart, which means missing or duplicated fragments."),
    ("TRF-013", "transfer", "source", "info", "retry", True,
     "Tarayıcı aktarımı kuyruk kotası, zaman veya eşzamanlılık sınırında durduruldu.",
     "Browser transfer stopped at the queue quota, time, or concurrency limit",
     "A browser transfer reached a queue policy limit; automatic resumption may wait for the policy to permit work."),
    # --- storage -------------------------------------------------------------------
    ("STO-001", "storage", "bridge", "error", "retry", True,
     "Kuyruk veritabanı hatası",
     "Queue database error",
     "A SQLite operation failed."),
    ("STO-002", "storage", "bridge", "error", "report", False,
     "Kayıtlı durum bozuk",
     "The stored state is corrupted",
     "Persisted state failed schema or invariant validation."),
    ("STO-003", "storage", "source", "warn", "settings", False,
     "Kuyruk kotası dolu",
     "The queue quota is full",
     "A queue quota prevents accepting more jobs."),
    ("STO-004", "storage", "bridge", "error", "report", False,
     "Ayar şeması geçersiz",
     "Invalid settings schema",
     "A settings payload did not match the expected schema."),
    ("STO-005", "storage", "source", "error", "settings", False,
     "Profil klasörüne erişilemedi",
     "The profile folder could not be accessed",
     "The per-user data directory is not writable or readable."),
    # --- recovery ------------------------------------------------------------------
    ("RCV-001", "recovery", "validation", "error", "report", True,
     "Yayın günlüğü (journal) uyuşmuyor",
     "The publication journal does not match",
     "A publication journal did not match the queue record."),
    ("RCV-002", "recovery", "validation", "error", "report", True,
     "Dosya kimliği doğrulanamadı",
     "The file identity could not be verified",
     "The file identity recorded before recovery no longer matches."),
    ("RCV-003", "recovery", "segment", "warn", "retry", True,
     "Yarım parça bulundu ve korundu",
     "A partial piece was found and kept",
     "A partial piece was kept for resumption instead of being discarded."),
    ("RCV-004", "recovery", "validation", "error", "report", True,
     "Kurtarma sahipliği çakıştı",
     "Recovery ownership conflicted",
     "Two recovery paths claimed the same output."),
    # --- update --------------------------------------------------------------------
    ("UPD-001", "update", "discovery", "error", "retry", True,
     "Güncelleme akışına erişilemedi",
     "The update feed could not be accessed",
     "The feed document could not be fetched or parsed."),
    ("UPD-002", "update", "validation", "error", "report", False,
     "Güncelleme akışı imzası doğrulanamadı",
     "The update feed signature could not be verified",
     "The feed signature did not verify against the embedded key; the update is refused."),
    ("UPD-003", "update", "validation", "error", "report", False,
     "İndirilen kurulumun SHA-256 değeri uyuşmadı",
     "The downloaded installer's SHA-256 does not match",
     "The downloaded installer digest differs from the signed feed."),
    ("UPD-004", "update", "output", "error", "report", True,
     "Kurulum yardımcısı başlatılamadı",
     "The install helper could not be started",
     "The detached install helper could not be started."),
    ("UPD-005", "update", "output", "warn", "report", True,
     "Karantinaya taşıma başarısız",
     "Moving to quarantine failed",
     "Moving a superseded artifact into quarantine failed."),
    ("UPD-006", "update", "discovery", "info", "retry", True,
     "Günlük denetim kapısı etkin",
     "The daily check gate is in effect",
     "The once-per-day check gate skipped this startup."),
    # --- extension side ------------------------------------------------------------
    ("EXT-001", "extension", "bridge", "warn", "settings", False,
     "Çerez izni verilmedi",
     "Cookie permission was not granted",
     "The optional cookie permission was not granted for this site."),
    ("EXT-002", "extension", "bridge", "warn", "refresh", True,
     "Eklenti bağlamı geçersiz (yeniden yüklendi)",
     "The extension context is stale (extension was reloaded)",
     "The page kept a content script from a previous extension load."),
    ("EXT-003", "extension", "bridge", "warn", "refresh", True,
     "Sekme kapandı",
     "The tab was closed",
     "The inspected tab no longer exists."),
    ("EXT-004", "extension", "bridge", "warn", "refresh", True,
     "Sayfa belgesi değişti",
     "The page document has changed",
     "The frame document changed since discovery."),
    ("EXT-005", "extension", "bridge", "warn", "refresh", True,
     "Kaynak değişti",
     "The source has changed",
     "The selected source is no longer present or its address changed."),
    ("EXT-006", "extension", "bridge", "error", "report", False,
     "İndirme bileti uyuşmadı",
     "The download ticket did not match",
     "The resumption ticket did not match the stored submission."),
    ("EXT-007", "extension", "bridge", "warn", "unsupported", False,
     "Tarayıcı aktarımı bu oynatıcı için kullanılamaz",
     "Browser relay is not available for this player",
     "Relay transfer needs a verified player frame with an observed manifest."),
    ("EXT-008", "extension", "bridge", "warn", "settings", False,
     "Bu kullanım amacı kapalı",
     "This usage mode is disabled",
     "The matching usage mode is disabled in Settings."),
    ("EXT-009", "extension", "bridge", "info", "retry", True,
     "İstek zaten kuyruğa alındı",
     "The request was already queued",
     "The same selection was already submitted."),
    ("EXT-010", "extension", "bridge", "warn", "refresh", True,
     "Sayfa taranamadı",
     "The page could not be scanned",
     "Page discovery did not produce usable candidates."),
]

SEVERITY = {"info": "Info", "warn": "Warn", "error": "Error"}
ACTION = {
    "retry": "Retry", "refresh": "Refresh", "open_app": "OpenApp", "open_page": "OpenPage",
    "settings": "Settings", "update": "Update", "unsupported": "Unsupported", "report": "Report",
}


def rust_ident(code: str) -> str:
    return code.replace("-", "_")


def rust() -> str:
    lines = [
        "//! Stable error codes for every user-visible failure.",
        "//!",
        "//! The table is the single source of truth: `browser/chromium/codes.js` mirrors the code set",
        "//! and a test fails when the two drift apart. Codes are permanent identifiers — never",
        "//! renumber or reuse one. README.md owns the user-visible contract and the actions; this",
        "//! table stays the authored source for the codes themselves.",
        "",
        "use std::fmt;",
        "",
        "/// One stable error code, e.g. `SSD-MED-002`.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]",
        "pub struct Code(pub &'static str);",
        "",
        "impl fmt::Display for Code {",
        "    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {",
        "        formatter.write_str(self.0)",
        "    }",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum Severity {",
        "    Info,",
        "    Warn,",
        "    Error,",
        "}",
        "",
        "/// What the caller (popup, dialog, CLI) should offer next.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub enum Action {",
        "    Retry,",
        "    Refresh,",
        "    OpenApp,",
        "    OpenPage,",
        "    Settings,",
        "    Update,",
        "    Unsupported,",
        "    Report,",
        "    None,",
        "}",
        "",
        "pub struct Definition {",
        "    pub code: Code,",
        "    pub area: &'static str,",
        "    pub stage: &'static str,",
        "    pub severity: Severity,",
        "    pub action: Action,",
        "    pub retryable: bool,",
        "    pub message_tr: &'static str,",
        "    pub message_en: &'static str,",
        "    pub description: &'static str,",
        "}",
        "",
        "macro_rules! define {",
        "    ($( $ident:ident => $code:literal, $area:literal, $stage:literal, $severity:ident, $action:ident, $retryable:literal, $message:literal, $message_en:literal, $description:literal; )*) => {",
        "        $( pub const $ident: Code = Code($code); )*",
        "",
        "        pub const TABLE: &[Definition] = &[",
        "            $( Definition {",
        "                code: $ident,",
        "                area: $area,",
        "                stage: $stage,",
        "                severity: Severity::$severity,",
        "                action: Action::$action,",
        "                retryable: $retryable,",
        "                message_tr: $message,",
        "                message_en: $message_en,",
        "                description: $description,",
        "            }, )*",
        "        ];",
        "    };",
        "}",
        "",
        "define! {",
    ]
    for code, area, stage, severity, action, retryable, message, message_en, description in CODES:
        lines.append(
            f'    {rust_ident(code)} => "SSD-{code}", "{area}", "{stage}", '
            f"{SEVERITY[severity]}, {ACTION[action]}, "
            f'{str(retryable).lower()}, "{message}", "{message_en}", "{description}";'
        )
    lines += [
        "}",
        "",
        "/// Definition for a code, or `None` when the code is unknown (older build).",
        "pub fn definition(code: Code) -> Option<&'static Definition> {",
        "    TABLE.iter().find(|entry| entry.code == code)",
        "}",
        "",
        "/// The message shown to a user, prefixed with the code so every surface reports it.",
        "pub fn labelled_message(code: Code) -> String {",
        "    match definition(code) {",
        '        Some(entry) if crate::i18n::english() => format!("{} {}", code, entry.message_en),',
        '        Some(entry) => format!("{} {}", code, entry.message_tr),',
        '        None => format!("{code} {}", crate::i18n::ui("Bilinmeyen hata kodu", "Unknown error code")),',
        "    }",
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::*;",
        "",
        "    #[test]",
        "    fn every_code_is_unique_and_documented() {",
        "        let mut codes: Vec<&str> = TABLE.iter().map(|entry| entry.code.0).collect();",
        "        let count = codes.len();",
        "        codes.sort_unstable();",
        "        codes.dedup();",
        "        assert_eq!(codes.len(), count, \"duplicate error code\");",
        "        for entry in TABLE {",
        "            assert!(",
        '                entry.code.0.starts_with("SSD-"),',
        '                "{} lacks the SSD- prefix",',
        "                entry.code",
        "            );",
        "            assert!(",
        "                !entry.message_tr.is_empty(),",
        '                "{} has no user message",',
        "                entry.code",
        "            );",
        "            assert!(",
        "                !entry.message_en.is_empty(),",
        '                "{} has no English user message",',
        "                entry.code",
        "            );",
        "            assert!(",
        "                !entry.description.is_empty(),",
        '                "{} has no description",',
        "                entry.code",
        "            );",
        "            assert!(",
        "                entry.message_tr.len() <= 220,",
        '                "{} message is too long",',
        "                entry.code",
        "            );",
        "        }",
        "    }",
        "",
        "    #[test]",
        "    fn areas_use_consecutive_numbers_without_gaps() {",
        "        let mut by_area: std::collections::BTreeMap<&str, Vec<u32>> =",
        "            std::collections::BTreeMap::new();",
        "        for entry in TABLE {",
        "            let number: u32 = entry",
        "                .code",
        "                .0",
        "                .rsplit('-')",
        "                .next()",
        "                .and_then(|value| value.parse().ok())",
        '                .unwrap_or_else(|| panic!("{} has no numeric suffix", entry.code));',
        "            by_area.entry(entry.area).or_default().push(number);",
        "        }",
        "        for (area, mut numbers) in by_area {",
        "            numbers.sort_unstable();",
        "            for (index, number) in numbers.iter().enumerate() {",
        "                assert_eq!(",
        "                    *number,",
        "                    index as u32 + 1,",
        '                    "{area} numbering has a gap or duplicate"',
        "                );",
        "            }",
        "        }",
        "    }",
        "",
        "    #[test]",
        "    fn ownership_names_who_can_fix_the_failure() {",
        "        assert_eq!(owner(MED_003), Owner::Drm);",
        "        assert_eq!(owner(MED_004), Owner::Drm);",
        "        assert_eq!(owner(MED_007), Owner::Site);",
        "        assert_eq!(owner(MED_008), Owner::Site);",
        "        assert_eq!(owner(EXT_007), Owner::Observation);",
        "        assert_eq!(owner(MED_006), Owner::App);",
        "        assert_eq!(Owner::App.as_str(), \"app\");",
        "        assert_eq!(Owner::Drm.as_str(), \"drm\");",
        "    }",
        "",
        "    #[test]",
        "    fn stored_text_round_trips_through_the_registry() {",
        "        let code = TABLE[4].code;",
        "        let error = coded(code, \"örnek hata\");",
        "        assert_eq!(from_text(&format!(\"{error:#}\")), Some(code));",
        "        assert_eq!(code_of_text(&error), Some(code));",
        "        assert_eq!(from_text(\"bilinmeyen bir hata\"), None);",
        "        assert_eq!(from_text(\"SSD-XXX-999 yabancı kod\"), None);",
        "    }",
        "",
        "    #[test]",
        "    fn the_extension_mirror_lists_exactly_the_same_codes() {",
        '        let mirror = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/browser/chromium/codes.js"))',
        '            .expect("browser/chromium/codes.js must exist");',
        "        for entry in TABLE {",
        "            assert!(",
        '                mirror.contains(&format!("\\"{}\\":", entry.code)),',
        '                "{} is missing from browser/chromium/codes.js",',
        "                entry.code",
        "            );",
        "        }",
        '        let listed = mirror.matches("\\"SSD-").count();',
        "        assert_eq!(",
        "            listed,",
        "            TABLE.len(),",
        '            "browser/chromium/codes.js lists a different number of codes than the registry"',
        "        );",
        "    }",
        "}",
        "",
        MACHINERY,
    ]
    return "\n".join(lines)


def javascript() -> str:
    out = [
        "// Stable error codes mirrored from src/error_codes.rs. The Rust test",
        "// `the_extension_mirror_lists_exactly_the_same_codes` fails when the two lists drift.",
        "// `action` tells the browser surface what to offer next: retry, refresh, open_app, open_page,",
        "// settings, update, unsupported, report.",
        "const ERROR_CODES = {",
    ]
    for code, area, stage, severity, action, retryable, message, message_en, description in CODES:
        out.append(f'  "SSD-{code}": {{ area: "{area}", stage: "{stage}", severity: "{severity}", action: "{action}", retryable: {"true" if retryable else "false"} }},')
    out += [
        "};",
        "",
        "function codeAction(code) {",
        "  if (!code) return null;",
        "  return ERROR_CODES[code]?.action || null;",
        "}",
        "",
        "function isCode(code) {",
        '  return typeof code === "string" && Object.prototype.hasOwnProperty.call(ERROR_CODES, code);',
        "}",
        "",
        "function codes() {",
        "  return Object.keys(ERROR_CODES);",
        "}",
        "",
        "// Splits a native message of the form `SSD-XXX-NNN rest of the text`.",
        "function splitCode(message) {",
        '  const text = String(message || "");',
        '  const match = text.match(/^(SSD-[A-Z]{3}-\\d{3})\\s*(.*)$/);',
        "  return match ? { code: match[1], text: match[2] } : { code: null, text };",
        "}",
        "",
        "if (typeof globalThis !== \"undefined\") {",
        "  globalThis.SSDownloadErrorCodes = { ERROR_CODES, codeAction, isCode, codes, splitCode };",
        "}",
        "",
    ]
    return "\n".join(out)


def readme_rows() -> str:
    rows = []
    for code, area, stage, severity, action, retryable, message, message_en, description in CODES:
        rows.append(f"| `SSD-{code}` | {message} | {message_en} | `{action}` | {description} |")
    return "\n".join(rows)


root = Path(__file__).resolve().parent.parent
(root / "src/error_codes.rs").write_text(rust(), encoding="utf-8", newline="\n")
(root / "browser/chromium/codes.js").write_text(javascript(), encoding="utf-8", newline="\n")
table = root / "target/error-codes-table.md"
table.parent.mkdir(exist_ok=True)
table.write_text(readme_rows() + "\n", encoding="utf-8", newline="\n")
format_rust(root / "src/error_codes.rs")
print(f"wrote {len(CODES)} codes: src/error_codes.rs, browser/chromium/codes.js, target/error-codes-table.md")
