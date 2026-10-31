//! SHA-256 digests that the source of an HTTP download publishes for the file, used to verify
//! the finished bytes when the user gave no checksum: the `Repr-Digest` header (RFC 9530) of
//! the response that delivered them, or a `.sha256` or `SHA256SUMS` file next to the address
//! the download was given.

use super::{
    build_headers, configure_easy, is_redirect, is_web_page, origin, redirected_url, url_file_name,
    validate_source_url, ResponseHeaders,
};
use crate::{
    model::{AddRequest, TransferControl},
    network::NetworkGovernor,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};
use url::Url;

/// Files smaller than this are not looked up next to their address: two more requests would
/// double the cost of fetching them, and a flaw in so few bytes is not worth the extra
/// round trips. A `Repr-Digest` header costs nothing and is used at any size.
pub(super) const MIN_LOOKUP_BYTES: u64 = 1024 * 1024;
const BODY_LIMIT: usize = 64 * 1024;
/// Requests, redirects included, that a lookup may send for one download.
const REQUEST_LIMIT: u8 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    ReprDigest,
    Sha256File,
    Sha256Sums,
}

impl Source {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ReprDigest => "Repr-Digest",
            Self::Sha256File => ".sha256",
            Self::Sha256Sums => "SHA256SUMS",
        }
    }
}

pub(super) struct Published {
    pub(super) sha256: String,
    pub(super) source: Source,
}

/// The record, beside a download's resume record, that its one re-download after a digest
/// mismatch is used up.
pub(super) fn retry_record(state: &Path) -> PathBuf {
    super::append_suffix(state, ".digest-retry")
}

/// Records that the re-download is used, durably. `false` when it was used already or the record
/// cannot be written: no re-download is then granted.
pub(super) fn grant_retry(record: &Path) -> bool {
    !record.exists() && crate::recovery::atomic_write(record, b"1").is_ok()
}

/// The `sha-256` member of a `Repr-Digest` field value, as lower-case hex. A repeated or
/// malformed member says nothing. The digest covers the representation as the server sends it,
/// with its content coding applied, which is what the engine stores: it asks for no coding and
/// decodes none.
pub(super) fn repr_sha256(value: &str) -> Option<String> {
    let mut found = None;
    for member in value.split(',') {
        let Some((key, item)) = member.trim().split_once('=') else {
            continue;
        };
        if key != "sha-256" {
            continue;
        }
        if found.is_some() {
            return None;
        }
        let item = item.split(';').next().unwrap_or(item).trim();
        let encoded = item.strip_prefix(':')?.strip_suffix(':')?;
        let bytes = STANDARD.decode(encoded).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        found = Some(hex::encode(bytes));
    }
    found
}

fn hex_digest(text: &str) -> Option<String> {
    (text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

/// The digest of a `<hex>  <name>` or `<hex> *<name>` line, when `name` is the file.
fn named_digest(line: &str, name: &str) -> Option<String> {
    let (digest, rest) = line.split_at_checked(64)?;
    let rest = rest.strip_prefix(' ')?;
    let file = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'))?;
    (file == name).then(|| hex_digest(digest)).flatten()
}

fn lines(body: &[u8]) -> Option<Vec<&str>> {
    let text = std::str::from_utf8(body).ok()?;
    Some(
        text.strip_prefix('\u{feff}')
            .unwrap_or(text)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect(),
    )
}

/// The digest in a `<file>.sha256` file: a bare digest, or the line for `name`. Anything else,
/// several lines among it, says nothing.
fn sha256_file_digest(body: &[u8], name: &str) -> Option<String> {
    match lines(body)?.as_slice() {
        [line] => hex_digest(line.trim()).or_else(|| named_digest(line, name)),
        _ => None,
    }
}

/// The digest on the one line of a `SHA256SUMS` file that names `name`.
fn sums_digest(body: &[u8], name: &str) -> Option<String> {
    let mut matches = lines(body)?
        .into_iter()
        .filter_map(|line| named_digest(line, name));
    let digest = matches.next()?;
    matches.next().is_none().then_some(digest)
}

/// What a digest lookup needs from the address a download was given: the file name, the
/// `.sha256` address and the `SHA256SUMS` address in the same directory. The query and the
/// fragment belong to the request for the file, not to its neighbours.
fn sidecar_addresses(original: &Url) -> Option<(String, Url, Url)> {
    let name = url_file_name(original)?;
    let mut base = original.clone();
    base.set_query(None);
    base.set_fragment(None);
    let path = base.path().to_owned();
    let directory = &path[..=path.rfind('/')?];
    let mut file = base.clone();
    file.set_path(&format!("{path}.sha256"));
    let mut sums = base;
    sums.set_path(&format!("{directory}SHA256SUMS"));
    Some((name, file, sums))
}

/// The body of a small `200` answer from the origin the download was given, following
/// redirects only within it. Every request counts against `budget`.
fn fetch(
    request: &AddRequest,
    first: Url,
    original_origin: &(String, String, u16),
    budget: &mut u8,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Option<Vec<u8>> {
    let mut current = first;
    loop {
        if control.stop_requested() || *budget == 0 {
            return None;
        }
        *budget -= 1;
        let mut easy = network.easy_for_url(&current, control).ok()?;
        configure_easy(&mut easy, &current).ok()?;
        easy.follow_location(false).ok()?;
        easy.progress(true).ok()?;
        easy.timeout(REQUEST_TIMEOUT).ok()?;
        easy.http_headers(build_headers(request, &current, original_origin, None).ok()?)
            .ok()?;
        let response = Rc::new(RefCell::new(ResponseHeaders::default()));
        let callback = response.clone();
        let body = Rc::new(RefCell::new(Vec::<u8>::new()));
        let write_response = response.clone();
        let write_body = body.clone();
        let overflow = Rc::new(RefCell::new(false));
        let write_overflow = overflow.clone();
        let performed = {
            let mut transfer = easy.transfer();
            transfer
                .header_function(move |line| {
                    super::parse_header_line(&mut callback.borrow_mut(), line);
                    true
                })
                .ok()?;
            transfer
                .write_function(move |data| {
                    let headers = write_response.borrow();
                    let mut body = write_body.borrow_mut();
                    if headers
                        .content_length
                        .is_some_and(|length| length > BODY_LIMIT as u64)
                        || body.len() + data.len() > BODY_LIMIT
                    {
                        *write_overflow.borrow_mut() = true;
                        return Ok(0);
                    }
                    if headers.status == 200 {
                        body.extend_from_slice(data);
                    }
                    Ok(data.len())
                })
                .ok()?;
            transfer
                .progress_function(|_, _, _, _| !control.stop_requested())
                .ok()?;
            transfer.perform()
        };
        let headers = response.borrow().clone();
        if is_redirect(headers.status) {
            let next = redirected_url(&current, headers.location.as_deref()).ok()?;
            if origin(&next) != *original_origin {
                return None;
            }
            current = next;
            continue;
        }
        let page = is_web_page(headers.content_type.as_deref(), headers.attachment);
        if performed.is_err() || *overflow.borrow() || headers.status != 200 || page {
            return None;
        }
        return Some(body.take());
    }
}

/// The digest that the source publishes for `bytes` downloaded for `request`: `header`, the
/// `Repr-Digest` of the response that delivered them, or else the one named by a `.sha256` file
/// and then by a `SHA256SUMS` file in the directory of the address the download was given, which
/// may differ from the address that served the bytes. Both are read from that address's origin
/// only, with the headers of the download, in at most `REQUEST_LIMIT` requests.
pub(super) fn published(
    request: &AddRequest,
    header: Option<String>,
    bytes: u64,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Option<Published> {
    if let Some(sha256) = header {
        return Some(Published {
            sha256,
            source: Source::ReprDigest,
        });
    }
    if bytes < MIN_LOOKUP_BYTES {
        return None;
    }
    let original = validate_source_url(&request.url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))?;
    let (name, file, sums) = sidecar_addresses(&original)?;
    let original_origin = origin(&original);
    let mut budget = REQUEST_LIMIT;
    let mut read = |url: Url| {
        fetch(
            request,
            url,
            &original_origin,
            &mut budget,
            control,
            network,
        )
    };
    if let Some(sha256) = read(file).and_then(|body| sha256_file_digest(&body, &name)) {
        return Some(Published {
            sha256,
            source: Source::Sha256File,
        });
    }
    let sha256 = read(sums).and_then(|body| sums_digest(&body, &name))?;
    Some(Published {
        sha256,
        source: Source::Sha256Sums,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn encoded(hex_digest: &str) -> String {
        STANDARD.encode(hex::decode(hex_digest).unwrap())
    }

    #[test]
    fn repr_digest_yields_the_sha256_member_and_nothing_unclean() {
        let member = format!("sha-256=:{}:", encoded(DIGEST));
        assert_eq!(repr_sha256(&member).as_deref(), Some(DIGEST));
        // Other algorithms around it, and a parameter on the member, do not matter.
        let sha512 = STANDARD.encode([7u8; 64]);
        let around = format!("sha-512=:{sha512}:, {member};x=1, md5=:AAAA:");
        assert_eq!(repr_sha256(&around).as_deref(), Some(DIGEST));
        // Several header lines arrive joined; the same digest twice is still ambiguous.
        assert_eq!(repr_sha256(&format!("{member}, {member}")), None);

        let short = format!("sha-256=:{}:", STANDARD.encode([1u8; 31]));
        let bare = format!("sha-256={}", encoded(DIGEST));
        let only_sha512 = format!("sha-512=:{sha512}:");
        for value in [
            short.as_str(),
            bare.as_str(),
            "sha-256=:!!!!:",
            only_sha512.as_str(),
            "sha-256",
            "",
        ] {
            assert_eq!(repr_sha256(value), None, "{value:?}");
        }
    }

    #[test]
    fn a_sha256_file_holds_a_bare_digest_or_the_one_line_naming_the_file() {
        let upper = DIGEST.to_ascii_uppercase();
        for body in [
            format!("{DIGEST}\n"),
            format!("  {DIGEST}  \r\n"),
            format!("{DIGEST}  file.iso\n"),
            format!("{DIGEST} *file.iso\r\n"),
            format!("\u{feff}{upper}  file.iso\n\n"),
        ] {
            assert_eq!(
                sha256_file_digest(body.as_bytes(), "file.iso").as_deref(),
                Some(DIGEST),
                "{body:?}"
            );
        }
        for body in [
            format!("{DIGEST}  other.iso\n"),
            format!("{DIGEST} file.iso\n"),
            format!("{DIGEST}  file.iso\n{DIGEST}  file.iso\n"),
            format!("{DIGEST}\n{DIGEST}\n"),
            format!("{}\n", &DIGEST[1..]),
            format!("{}g\n", &DIGEST[1..]),
            format!("{DIGEST}  file.iso extra\n"),
            "<html>not found</html>".to_owned(),
            String::new(),
        ] {
            assert_eq!(
                sha256_file_digest(body.as_bytes(), "file.iso"),
                None,
                "{body:?}"
            );
        }
        assert_eq!(sha256_file_digest(&[0xff, 0xfe], "file.iso"), None);
    }

    #[test]
    fn a_sums_file_yields_the_one_line_whose_name_is_exactly_the_file() {
        let other = "0".repeat(64);
        let sums = format!(
            "{other}  file.iso.sig\n{DIGEST}  file.iso\n{other} *xfile.iso\n{other}  FILE.ISO\n"
        );
        assert_eq!(
            sums_digest(sums.as_bytes(), "file.iso").as_deref(),
            Some(DIGEST)
        );
        assert_eq!(sums_digest(sums.as_bytes(), "missing.iso"), None);
        // Two lines name the file: which one is right is unknown.
        let twice = format!("{other}  file.iso\n{DIGEST}  file.iso\n");
        assert_eq!(sums_digest(twice.as_bytes(), "file.iso"), None);
        // A bare digest names nothing.
        assert_eq!(
            sums_digest(format!("{DIGEST}\n").as_bytes(), "file.iso"),
            None
        );
    }

    #[test]
    fn digest_files_are_looked_up_beside_the_address_without_its_query() {
        let url = |value: &str| Url::parse(value).unwrap();
        let (name, file, sums) =
            sidecar_addresses(&url("https://example.test/a/b/My%20File.iso?token=1#top")).unwrap();
        assert_eq!(name, "My File.iso");
        assert_eq!(
            file.as_str(),
            "https://example.test/a/b/My%20File.iso.sha256"
        );
        assert_eq!(sums.as_str(), "https://example.test/a/b/SHA256SUMS");

        let (name, file, sums) = sidecar_addresses(&url("http://example.test:8080/f.bin")).unwrap();
        assert_eq!(name, "f.bin");
        assert_eq!(file.as_str(), "http://example.test:8080/f.bin.sha256");
        assert_eq!(sums.as_str(), "http://example.test:8080/SHA256SUMS");

        // An address without a file name has no neighbours to ask.
        assert!(sidecar_addresses(&url("https://example.test/dir/")).is_none());
        assert!(sidecar_addresses(&url("https://example.test")).is_none());
    }
}
