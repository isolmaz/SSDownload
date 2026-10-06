//! Summary of the recorded event log: what happened, what failed and whose fault it was.
//!
//! The summary reads the `logs/events-*.jsonl` files and never touches the log itself, so it
//! works while the desktop is running and after a crash. It answers the question the log
//! exists for: which sites failed, with which codes, in the last N hours.

use crate::paths::AppPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Count {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    pub hours: i64,
    pub files: usize,
    pub events: u64,
    pub failures: u64,
    pub dropped: u64,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_outcome: BTreeMap<String, u64>,
    /// `app`, `site`, `drm` or `observation`: who can resolve the failures.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_owner: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_codes: Vec<Count>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_hosts: Vec<Count>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_events: Vec<Count>,
}

const TOP: usize = 10;

/// Reads the session log files and aggregates the last `hours` of activity.
pub fn summary(paths: &AppPaths, hours: i64) -> Result<Summary> {
    let logs = paths.base_dir.join("logs");
    let cutoff = chrono::Local::now() - chrono::Duration::hours(hours);
    let mut summary = Summary {
        hours,
        ..Summary::default()
    };
    let mut codes: BTreeMap<String, u64> = BTreeMap::new();
    let mut hosts: BTreeMap<String, u64> = BTreeMap::new();
    let mut events: BTreeMap<String, u64> = BTreeMap::new();
    let mut files = Vec::new();
    if logs.is_dir() {
        for entry in std::fs::read_dir(&logs).context("Günlük klasörü okunamadı")? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    // The window can span a UTC-offset change (a session recorded in another time zone, or
    // a daylight-saving switch elsewhere), so the bounds are tracked as instants and only
    // formatted once at the end: formatted text compared as a string can invert the order.
    let mut first: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut last: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        summary.files += 1;
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(stamp) = value
                .get("ts")
                .and_then(|value| value.as_str())
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            else {
                continue;
            };
            if stamp.with_timezone(&chrono::Local) < cutoff {
                continue;
            }
            summary.events += 1;
            first = Some(first.map_or(stamp, |value| value.min(stamp)));
            last = Some(last.map_or(stamp, |value| value.max(stamp)));
            if let Some(name) = value.get("event").and_then(|value| value.as_str()) {
                *events.entry(name.to_owned()).or_default() += 1;
            }
            if let Some(outcome) = value.get("outcome").and_then(|value| value.as_str()) {
                *summary.by_outcome.entry(outcome.to_owned()).or_default() += 1;
                if outcome == "failed" {
                    summary.failures += 1;
                }
            }
            if let Some(code) = value.get("code").and_then(|value| value.as_str()) {
                *codes.entry(code.to_owned()).or_default() += 1;
            }
            if let Some(owner) = value.get("owner").and_then(|value| value.as_str()) {
                *summary.by_owner.entry(owner.to_owned()).or_default() += 1;
            }
            if let Some(host) = value.get("host").and_then(|value| value.as_str()) {
                *hosts.entry(host.to_owned()).or_default() += 1;
            }
            if value.get("event").and_then(|value| value.as_str()) == Some("log.dropped") {
                if let Some(detail) = value.get("detail").and_then(|value| value.as_str()) {
                    if let Some(number) = detail.split_whitespace().next() {
                        summary.dropped += number.parse::<u64>().unwrap_or(0);
                    }
                }
            }
        }
    }
    summary.dropped = summary.dropped.max(crate::logging::dropped_events());
    summary.first_ts = first.map(|value| value.to_rfc3339());
    summary.last_ts = last.map(|value| value.to_rfc3339());
    summary.top_codes = top(codes);
    summary.top_hosts = top(hosts);
    summary.top_events = top(events);
    Ok(summary)
}

fn top(values: BTreeMap<String, u64>) -> Vec<Count> {
    let mut counts: Vec<Count> = values
        .into_iter()
        .map(|(name, count)| Count { name, count })
        .collect();
    counts.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then(left.name.cmp(&right.name))
    });
    counts.truncate(TOP);
    counts
}

/// Builds one shareable ZIP with the job metadata, the session summary and the recent logs.
///
/// There is no delete step: every package is a new timestamped file, and the per-day log files
/// it copies stay where they are.
/// A written diagnostics package: readable files in a folder plus the same snapshot as one ZIP.
#[derive(Debug, Clone)]
pub struct Package {
    pub folder: std::path::PathBuf,
    pub zip: std::path::PathBuf,
}

/// Writes the job metadata, the session summary, the recent logs and the job timelines into
/// `SSDownload-teshis-<version>-<stamp>/` and the same entries into `…-<stamp>.zip`.
///
/// The files are copies, so the evidence cannot change while the package is read; there is no
/// delete step and a second package in the same minute gets a `-2` suffix.
pub fn package(paths: &AppPaths, jobs: &[serde_json::Value], hours: i64) -> Result<Package> {
    let entries = collect_entries(paths, jobs, hours)?;
    write_package(paths, "SSDownload-teshis", entries)
}

/// Everything a report carries besides the failure itself. The page HTML is the
/// one field that can hold site-bound markup, so it is written as its own file
/// and never uploaded: the whole report stays under the profile.
#[derive(Debug, Clone, Default)]
pub struct ReportContext {
    /// Stable registry code the failure was classified with, when it has one.
    pub code: Option<String>,
    /// The user-facing failure text exactly as the window showed it.
    pub message: String,
    pub job_id: Option<String>,
    pub job_name: Option<String>,
    /// Page the attempt came from. Stored redacted (no query, no fragment).
    pub site_url: Option<String>,
    pub page_title: Option<String>,
    /// Bounded page markup captured at handoff time, when the browser supplied it.
    pub page_html: Option<String>,
    /// Where that markup came from: `handoff` (the browser captured the page the
    /// user clicked in) or `desktop` (this process read the page itself because
    /// the browser supplied nothing). `None` when the report carries no markup.
    pub page_html_source: Option<&'static str>,
    /// Response status the desktop read answered with; the browser handoff does
    /// not record one.
    pub page_html_status: Option<u16>,
}

/// Longest page markup a report carries. The browser bounds its own capture with
/// the same number, so a desktop read is truncated to the same size.
pub const PAGE_MARKUP_LIMIT: usize = 192 * 1024;

/// Largest response body the desktop reads while filling in a report. It is a
/// read bound only: page markup is evidence, never a rendering input.
const PAGE_FETCH_LIMIT: usize = 512 * 1024;

/// Page markup read by the desktop when the browser handed none over, with the
/// response status that carried it. A failing page's own error body is exactly
/// the evidence a report wants, so no status is filtered out.
pub(crate) struct PageMarkup {
    pub html: String,
    pub status: u16,
}

/// Reads the page a failure came from, so a report carries markup the browser
/// did not hand over. The address is the redacted one the report stores (query
/// and fragment are gone), the request carries no credentials, no cookies and no
/// referer, and it goes to that page and nowhere else: the report itself still
/// uploads nothing, and a failure to read the page only means the report has no
/// `sayfa.html`.
pub(crate) fn fetch_page_markup(
    network: &crate::network::NetworkGovernor,
    url: &str,
) -> Option<PageMarkup> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return None;
    }
    let mut easy = network.wildcard_easy(&Default::default()).ok()?;
    restrict_to_http(&mut easy).ok()?;
    easy.url(parsed.as_str()).ok()?;
    easy.useragent(crate::tools::USER_AGENT).ok()?;
    easy.follow_location(true).ok()?;
    easy.max_redirections(5).ok()?;
    easy.fail_on_error(false).ok()?;
    easy.connect_timeout(std::time::Duration::from_secs(8))
        .ok()?;
    easy.timeout(std::time::Duration::from_secs(15)).ok()?;
    easy.ssl_verify_peer(true).ok()?;
    easy.ssl_verify_host(true).ok()?;
    let mut body: Vec<u8> = Vec::new();
    let mut too_large = false;
    {
        let mut transfer = easy.transfer();
        transfer
            .write_function(|chunk| {
                if body.len().saturating_add(chunk.len()) > PAGE_FETCH_LIMIT {
                    too_large = true;
                    return Ok(0);
                }
                body.extend_from_slice(chunk);
                Ok(chunk.len())
            })
            .ok()?;
        transfer.perform().ok()?;
    }
    if too_large {
        // The head of the page is still the useful part: keep what was read.
        body.truncate(PAGE_FETCH_LIMIT);
    }
    let status = easy.response_code().ok()?;
    let text = String::from_utf8_lossy(&body);
    Some(PageMarkup {
        html: bounded_markup(&strip_active_content(&text)),
        status: u16::try_from(status).unwrap_or_default(),
    })
}

/// Restricts one transfer to http and https, so a page address can never select
/// another protocol.
fn restrict_to_http(easy: &mut crate::network::GovernedEasy) -> Result<()> {
    let protocol = (curl_sys::CURLPROTO_HTTP | curl_sys::CURLPROTO_HTTPS) as std::os::raw::c_long;
    for option in [
        curl_sys::CURLOPT_PROTOCOLS,
        curl_sys::CURLOPT_REDIR_PROTOCOLS,
    ] {
        let code = unsafe { curl_sys::curl_easy_setopt(easy.raw(), option, protocol) };
        if code != curl_sys::CURLE_OK {
            bail!("Sayfa okuması için aktarım protokol sınırı uygulanamadı");
        }
    }
    Ok(())
}

/// Drops what a report never needs and would rather not carry: script bodies,
/// style sheets and comments. Page markup is evidence, so the rest is kept
/// verbatim.
fn strip_active_content(html: &str) -> String {
    const BLOCKS: [(&str, &str); 2] = [("<script", "</script>"), ("<style", "</style>")];
    let mut out = String::with_capacity(html.len().min(PAGE_MARKUP_LIMIT));
    let mut cursor = 0;
    while cursor < html.len() {
        let next = BLOCKS
            .iter()
            .filter_map(|(open, close)| {
                find_ignore_ascii_case(html, open, cursor).map(|start| {
                    (
                        start,
                        find_ignore_ascii_case(html, close, start).map(|end| end + close.len()),
                    )
                })
            })
            .chain(find_ignore_ascii_case(html, "<!--", cursor).map(|start| {
                (
                    start,
                    find_ignore_ascii_case(html, "-->", start).map(|end| end + 3),
                )
            }))
            .min_by_key(|(start, _)| *start);
        let Some((start, end)) = next else {
            break;
        };
        out.push_str(&html[cursor..start]);
        cursor = end.unwrap_or(html.len());
    }
    out.push_str(&html[cursor.min(html.len())..]);
    out
}

/// Truncates to [`PAGE_MARKUP_LIMIT`] on a character boundary, marking the cut.
fn bounded_markup(html: &str) -> String {
    if html.len() <= PAGE_MARKUP_LIMIT {
        return html.to_owned();
    }
    let mut end = PAGE_MARKUP_LIMIT;
    while end > 0 && !html.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = html[..end].to_owned();
    out.push_str("\n<!-- sayfa işaretlemesi kısaltıldı -->");
    out
}

/// Byte offset of `needle` in `haystack` at or after `from`, ignoring ASCII case.
fn find_ignore_ascii_case(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pin = needle.as_bytes();
    if pin.is_empty() || hay.len() < pin.len() || from > hay.len() - pin.len() {
        return None;
    }
    (from..=hay.len() - pin.len())
        .find(|&index| hay[index..index + pin.len()].eq_ignore_ascii_case(pin))
}

/// Writes a failure report: the diagnostics package plus `hata.json` (what the
/// user was doing) and, when the browser handed one over, `sayfa.html`.
///
/// Nothing here leaves the machine; the report exists so the user can attach it
/// to a bug report by hand.
pub fn report(
    paths: &AppPaths,
    jobs: &[serde_json::Value],
    hours: i64,
    context: &ReportContext,
) -> Result<Package> {
    let mut entries = collect_entries(paths, jobs, hours)?;
    let html_bytes = context.page_html.as_ref().map(|html| html.len());
    entries.push((
        "hata.json".to_owned(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "reported_at": chrono::Local::now().to_rfc3339(),
            "code": context.code,
            "message": context.message,
            "job_id": context.job_id,
            "job_name": context.job_name,
            "site_url": context.site_url,
            "page_title": context.page_title,
            "page_html": html_bytes.map(|bytes| serde_json::json!({
                "file": "sayfa.html",
                "bytes": bytes,
                "source": context.page_html_source,
                "status": context.page_html_status,
                "note": "Sayfa işaretlemesi oturuma bağlı veri içerebilir; rapor yalnız bu klasörde durur.",
            })),
        }))?,
    ));
    if let Some(html) = &context.page_html {
        entries.push(("sayfa.html".to_owned(), html.as_bytes().to_vec()));
    }
    write_package(paths, "SSDownload-hata", entries)
}

/// The shared payload: job metadata, the session summary, the system block and
/// the newest logs and job timelines.
fn collect_entries(
    paths: &AppPaths,
    jobs: &[serde_json::Value],
    hours: i64,
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    entries.push((
        "diagnostics.json".to_owned(),
        serde_json::to_vec_pretty(jobs)?,
    ));
    entries.push((
        "session.json".to_owned(),
        serde_json::to_vec_pretty(&summary(paths, hours)?)?,
    ));
    entries.push((
        "system.json".to_owned(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "session": crate::logging::session_id(),
            "level": crate::logging::level().map(|level| level.as_str()),
            "dropped_events": crate::logging::dropped_events(),
        }))?,
    ));
    let logs = paths.base_dir.join("logs");
    if logs.is_dir() {
        // Only the newest entries of each directory enter the package, so a long history
        // cannot push the interesting days out. Session logs are day-named and chronological
        // by name; job timelines are named `job-<uuid>.jsonl`, whose order is arbitrary, so
        // their recency has to come from the modification time, read once per file and
        // compared at full resolution (several timelines can share one second).
        for (dir, prefix, keep, by_time) in [
            (logs.clone(), "logs/", 4usize, false),
            (logs.join("jobs"), "logs/jobs/", 50usize, true),
        ] {
            let Ok(listing) = std::fs::read_dir(&dir) else {
                continue;
            };
            // One vector carries the selection key and the path, so nothing is rebuilt and
            // each entry's metadata is read once; that read filters out anything that is not
            // a file in both directories, while only the directory whose recency decides the
            // selection asks it for the modification time. A file whose modification time
            // cannot be read carries no key and sorts last, which keeps the previous
            // best-effort behaviour (an entry whose metadata cannot be read at all is
            // filtered out, as before).
            let mut files: Vec<(Option<std::time::SystemTime>, std::path::PathBuf)> = listing
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let meta = std::fs::metadata(&path).ok()?;
                    if !meta.is_file() {
                        return None;
                    }
                    let modified = if by_time { meta.modified().ok() } else { None };
                    Some((modified, path))
                })
                .collect();
            // Both orders are total: the paths are unique within a directory and the
            // recency order breaks its ties with the path, so an unstable sort is enough
            // and avoids the stable sort's temporary allocation.
            if by_time {
                files.sort_unstable_by(|left, right| {
                    right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1))
                });
            } else {
                files.sort_unstable_by(|left, right| right.1.cmp(&left.1));
            }
            for (_, path) in files.iter().take(keep) {
                if let (Some(name), Ok(bytes)) = (
                    path.file_name().and_then(|name| name.to_str()),
                    std::fs::read(path),
                ) {
                    entries.push((format!("{prefix}{name}"), bytes));
                }
            }
        }
    }

    Ok(entries)
}

/// Writes one folder plus the same contents as a ZIP under a shared stem.
fn write_package(
    paths: &AppPaths,
    prefix: &str,
    entries: Vec<(String, Vec<u8>)>,
) -> Result<Package> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M").to_string();
    let stem = format!("{prefix}-{}-{stamp}", env!("CARGO_PKG_VERSION"));
    let (folder, zip_path) = free_names(&paths.base_dir, &stem);
    for (name, bytes) in &entries {
        let target = folder.join(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)
            .with_context(|| format!("Teşhis paketi yazılamadı: {}", target.display()))?;
    }
    let file = std::fs::File::create(&zip_path)
        .with_context(|| format!("Teşhis paketi oluşturulamadı: {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &entries {
        zip.start_file(name.as_str(), options)?;
        std::io::Write::write_all(&mut zip, bytes)?;
    }
    zip.finish().context("Teşhis paketi tamamlanamadı")?;
    Ok(Package {
        folder,
        zip: zip_path,
    })
}

/// The folder and the ZIP share one stem; a name already in use gets `-2`, `-3`, ...
fn free_names(base: &std::path::Path, stem: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    for suffix in 1..1000 {
        let candidate = if suffix == 1 {
            stem.to_owned()
        } else {
            format!("{stem}-{suffix}")
        };
        let folder = base.join(&candidate);
        let zip = base.join(format!("{candidate}.zip"));
        if !folder.exists() && !zip.exists() {
            return (folder, zip);
        }
    }
    let folder = base.join(format!("{stem}-{}", uuid::Uuid::new_v4()));
    let zip = folder.with_extension("zip");
    (folder, zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_report_carries_the_context_markup_and_the_diagnostics_payload() {
        let base = std::env::temp_dir().join(format!("ssdownload-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(base.join("logs")).unwrap();
        let paths = AppPaths::new(base.clone()).unwrap();
        let html = "<html><body>oynatıcı</body></html>";
        let context = ReportContext {
            code: Some("SSD-MED-015".into()),
            message: "Seçilen çözünürlük artık yok".into(),
            job_id: Some("job-1".into()),
            job_name: Some("klip".into()),
            site_url: Some("https://site.example".into()),
            page_title: None,
            page_html: Some(html.into()),
            page_html_source: Some("handoff"),
            page_html_status: None,
        };
        let written = report(
            &paths,
            &[serde_json::json!({"error_code": "SSD-MED-015"})],
            24,
            &context,
        )
        .unwrap();

        let name = written
            .folder
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(&format!("SSDownload-hata-{}-", env!("CARGO_PKG_VERSION"))),
            "{name}"
        );
        // The failure itself is readable without opening the archive.
        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(written.folder.join("hata.json")).unwrap())
                .unwrap();
        assert_eq!(meta["code"], "SSD-MED-015");
        assert_eq!(meta["job_id"], "job-1");
        assert_eq!(meta["site_url"], "https://site.example");
        assert_eq!(meta["page_html"]["file"], "sayfa.html");
        assert_eq!(meta["page_html"]["bytes"], html.len());
        assert_eq!(meta["page_html"]["source"], "handoff");
        assert_eq!(
            std::fs::read(written.folder.join("sayfa.html")).unwrap(),
            html.as_bytes()
        );
        // The diagnostics payload travels with it, and the ZIP mirrors the folder.
        assert!(written.folder.join("session.json").is_file());
        assert!(written.folder.join("diagnostics.json").is_file());
        let archive_file = std::fs::File::open(&written.zip).unwrap();
        let mut archive = zip::ZipArchive::new(archive_file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect();
        for expected in [
            "hata.json",
            "sayfa.html",
            "session.json",
            "diagnostics.json",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} eksik: {names:?}"
            );
        }

        // A report without browser markup still carries the failure, and never
        // invents an empty page file.
        let bare = report(
            &paths,
            &[],
            24,
            &ReportContext {
                message: "Analiz başarısız".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(bare.folder.join("hata.json").is_file());
        assert!(!bare.folder.join("sayfa.html").exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_package_carries_the_summary_logs_and_job_timelines() {
        let base =
            std::env::temp_dir().join(format!("ssdownload-package-{}", uuid::Uuid::new_v4()));
        let logs = base.join("logs");
        let jobs = logs.join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(
            logs.join("events-20260101.jsonl"),
            "{\"ts\":\"2026-01-01T00:00:00.000+00:00\",\"event\":\"job.done\"}\n",
        )
        .unwrap();
        std::fs::write(logs.join("ssdownload-20260101.log"), "satır\n").unwrap();
        std::fs::write(jobs.join("job-abc.jsonl"), "{\"event\":\"job.stage\"}\n").unwrap();

        let paths = AppPaths::new(base.clone()).unwrap();
        let written = package(&paths, &[serde_json::json!({"error_code": null})], 24).unwrap();

        let name = written
            .folder
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(&format!("SSDownload-teshis-{}-", env!("CARGO_PKG_VERSION"))),
            "{name}"
        );
        assert_eq!(
            written.zip.file_stem().unwrap(),
            written.folder.file_name().unwrap()
        );

        // The folder is the readable copy: the same entries are present as plain files.
        let expected = [
            "diagnostics.json",
            "session.json",
            "system.json",
            "logs/events-20260101.jsonl",
            "logs/jobs/job-abc.jsonl",
        ];
        for relative in expected {
            let file = written.folder.join(relative);
            assert!(file.is_file(), "{} eksik", file.display());
        }

        let archive_file = std::fs::File::open(&written.zip).unwrap();
        let mut archive = zip::ZipArchive::new(archive_file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_owned())
            .collect();
        for relative in expected {
            assert!(
                names.iter().any(|name| name == relative),
                "{relative} eksik: {names:?}"
            );
        }

        // A second package in the same minute never overwrites the first one.
        let second = package(&paths, &[], 24).unwrap();
        assert_ne!(second.folder, written.folder);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_summary_counts_codes_hosts_and_owners_within_the_window() {
        let base =
            std::env::temp_dir().join(format!("ssdownload-diagnose-{}", uuid::Uuid::new_v4()));
        let logs = base.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let now = chrono::Local::now();
        let stamp = |offset_minutes: i64| {
            (now - chrono::Duration::minutes(offset_minutes))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
        };
        let lines = [
            format!(
                "{{\"ts\":\"{}\",\"session\":\"a\",\"channel\":\"app\",\"event\":\"job.fail\",\"level\":\"error\",\"code\":\"SSD-MED-007\",\"owner\":\"site\",\"outcome\":\"failed\",\"host\":\"site.example\"}}",
                stamp(5)
            ),
            format!(
                "{{\"ts\":\"{}\",\"session\":\"a\",\"channel\":\"app\",\"event\":\"job.fail\",\"level\":\"error\",\"code\":\"SSD-MED-007\",\"owner\":\"site\",\"outcome\":\"failed\",\"host\":\"site.example\"}}",
                stamp(6)
            ),
            format!(
                "{{\"ts\":\"{}\",\"session\":\"a\",\"channel\":\"app\",\"event\":\"job.done\",\"level\":\"info\",\"outcome\":\"ok\",\"host\":\"cdn.example\"}}",
                stamp(7)
            ),
            // outside the window
            format!(
                "{{\"ts\":\"{}\",\"session\":\"a\",\"channel\":\"app\",\"event\":\"job.fail\",\"level\":\"error\",\"code\":\"SSD-TRF-003\",\"owner\":\"app\",\"outcome\":\"failed\"}}",
                stamp(60 * 30)
            ),
        ];
        std::fs::write(logs.join("events-20260101.jsonl"), lines.join("\n")).unwrap();

        let summary = summary(&AppPaths::new(base.clone()).unwrap(), 24).unwrap();
        assert_eq!(summary.events, 3, "{summary:?}");
        assert_eq!(summary.failures, 2);
        assert_eq!(summary.by_outcome.get("ok"), Some(&1));
        assert_eq!(summary.by_owner.get("site"), Some(&2));
        assert_eq!(
            summary.top_codes.first().map(|entry| entry.name.as_str()),
            Some("SSD-MED-007")
        );
        assert_eq!(summary.top_codes.first().map(|entry| entry.count), Some(2));
        assert_eq!(
            summary.top_hosts.first().map(|entry| entry.name.as_str()),
            Some("site.example")
        );
        assert!(summary
            .top_codes
            .iter()
            .all(|entry| entry.name != "SSD-TRF-003"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_package_keeps_the_newest_job_timelines() {
        let base = std::env::temp_dir().join(format!("ssdownload-recent-{}", uuid::Uuid::new_v4()));
        let jobs = base.join("logs").join("jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        // Timeline names carry a random uuid, so the newest one deliberately sorts last by
        // name: only its modification time can select it.
        let now = std::time::SystemTime::now();
        for index in 0..60u64 {
            let name = format!("job-{index:04}.jsonl");
            let file = jobs.join(&name);
            std::fs::write(
                &file,
                format!("{{\"event\":\"job.stage\",\"index\":{index}}}\n"),
            )
            .unwrap();
            let age = if index == 0 { 0 } else { 3600 + index * 60 };
            let stamp = now - std::time::Duration::from_secs(age);
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(stamp)
                .unwrap();
        }

        let paths = AppPaths::new(base.clone()).unwrap();
        let written = package(&paths, &[], 24).unwrap();
        assert!(
            written.folder.join("logs/jobs/job-0000.jsonl").is_file(),
            "the newest timeline must be in the package"
        );
        let kept = std::fs::read_dir(written.folder.join("logs/jobs"))
            .unwrap()
            .count();
        assert_eq!(kept, 50, "the package keeps the newest 50 timelines");

        // A burst of timelines inside one second: the whole-second part is identical, so the
        // sub-second modification time has to decide which 50 are kept. Name order would keep
        // the wrong one, because the newest file is named last.
        let base_second = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(
                now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - 2,
            );
        std::fs::remove_dir_all(&jobs).unwrap();
        std::fs::create_dir_all(&jobs).unwrap();
        for index in 0..51u64 {
            let file = jobs.join(format!("job-9{index:03}.jsonl"));
            std::fs::write(&file, b"{\"event\":\"job.stage\"}\n").unwrap();
            let stamp = base_second + std::time::Duration::from_millis(500 - index * 10);
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(stamp)
                .unwrap();
        }
        let second = package(&paths, &[], 24).unwrap();
        assert!(
            second.folder.join("logs/jobs/job-9000.jsonl").is_file(),
            "sub-second recency picks the newest timeline of a one-second burst"
        );
        assert!(
            !second.folder.join("logs/jobs/job-9050.jsonl").is_file(),
            "the oldest timeline of the burst is the one left out"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_summary_reports_first_and_last_instant_across_offsets() {
        let base = std::env::temp_dir().join(format!("ssdownload-offset-{}", uuid::Uuid::new_v4()));
        let logs = base.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // Both instants are recent, so the ordinary 24 h window covers them. They are shown in
        // different UTC offsets, which gives them the opposite text order: the earlier instant
        // renders as a later wall clock.
        let now = chrono::Utc::now();
        let east = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let earlier = (now - chrono::Duration::hours(2)).with_timezone(&east);
        let later = (now - chrono::Duration::hours(1)).with_timezone(&chrono::Utc);
        let expected_first = earlier.to_rfc3339();
        let expected_last = later.to_rfc3339();
        assert!(
            expected_first.as_str() > expected_last.as_str(),
            "the fixture must invert the text order: {expected_first} > {expected_last}"
        );
        let lines = [
            format!(
                "{{\"ts\":\"{expected_first}\",\"event\":\"job.fail\",\"outcome\":\"failed\"}}"
            ),
            format!("{{\"ts\":\"{expected_last}\",\"event\":\"job.done\",\"outcome\":\"ok\"}}"),
        ];
        std::fs::write(logs.join("events-probe.jsonl"), lines.join("\n")).unwrap();

        let summary = summary(&AppPaths::new(base.clone()).unwrap(), 24).unwrap();
        assert_eq!(summary.events, 2, "{summary:?}");
        assert_eq!(
            summary.first_ts.as_deref(),
            Some(expected_first.as_str()),
            "the earliest instant belongs to first_ts"
        );
        assert_eq!(
            summary.last_ts.as_deref(),
            Some(expected_last.as_str()),
            "the latest instant belongs to last_ts"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// A report that has to read the page itself must not carry script bodies or
    /// style sheets, and must not be able to select another protocol.
    #[test]
    fn desktop_page_markup_is_read_bounded_and_without_active_content() {
        use std::io::{Read as _, Write as _};
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut served = 0;
            while served < 2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buffer = [0u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                if !request.starts_with("GET ") {
                    // A reserved connection that has not asked for anything yet.
                    continue;
                }
                served += 1;
                let (status, body) = if request.starts_with("GET /page") {
                    (
                        "200 OK",
                        "<html><head><style>body{color:red}</style></head><body>oynatıcı\
                         <script src=\"tracker.js\"></script><!-- gizli -->son</body></html>",
                    )
                } else {
                    ("404 Not Found", "yok")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });

        let governor = crate::network::NetworkGovernor::new(4);
        let base = format!("http://{address}");
        let page = fetch_page_markup(&governor, &format!("{base}/page")).expect("sayfa okunmalı");
        assert_eq!(page.status, 200);
        let markup = page.html;
        assert!(
            markup.contains("oynatıcı") && markup.contains("son"),
            "{markup}"
        );
        assert!(!markup.contains("tracker"), "{markup}");
        assert!(!markup.contains("color:red"), "{markup}");
        assert!(!markup.contains("gizli"), "{markup}");

        // A failing page's own body is the evidence a report wants, so it is
        // carried with the status instead of being dropped; an address that is
        // not a page at all leaves the report without markup.
        let missing = fetch_page_markup(&governor, &format!("{base}/missing")).expect("gövde");
        assert_eq!(missing.status, 404);
        assert_eq!(missing.html, "yok");
        assert!(fetch_page_markup(&governor, "file:///C:/gizli.html").is_none());
        assert!(fetch_page_markup(&governor, "ftp://127.0.0.1/gizli.html").is_none());
        server.join().unwrap();
    }

    #[test]
    fn desktop_page_markup_is_truncated_on_a_character_boundary() {
        let long = format!("{}son", "ı".repeat(PAGE_MARKUP_LIMIT));
        let bounded = bounded_markup(&long);
        assert!(bounded.len() < long.len());
        assert!(bounded.starts_with('ı'));
        assert!(bounded.ends_with("kısaltıldı -->"));
        assert!(bounded.is_char_boundary(bounded.len()));
        // Markup that fits is carried verbatim.
        assert_eq!(bounded_markup("<p>kısa</p>"), "<p>kısa</p>");
    }
}
