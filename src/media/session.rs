//! Session material for the tools: sealed inputs, scoped cookies, cookie jars and config quoting.

use super::*;

pub(super) struct SensitiveInputs {
    pub(super) config: Vec<u8>,
    pub(super) redactions: Vec<String>,
    pub(super) cookie_jar: Option<PathBuf>,
}
impl Drop for SensitiveInputs {
    fn drop(&mut self) {
        self.config.fill(0);
        if let Some(path) = &self.cookie_jar {
            let _ = fs::remove_file(path);
        }
    }
}
impl SensitiveInputs {
    pub(super) fn create(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        referer: Option<&str>,
        page_url: Option<&str>,
        cookies: &[ScopedCookie],
    ) -> Result<Self> {
        Self::create_for_download(url, headers, referer, page_url, cookies, true)
    }

    pub(super) fn create_for_download(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        referer: Option<&str>,
        page_url: Option<&str>,
        cookies: &[ScopedCookie],
        include_url: bool,
    ) -> Result<Self> {
        crate::validation::validate_headers(headers)?;
        reject_unsafe_media_headers(headers)?;
        validate_web_url(url)?;
        if let Some(referer) = referer {
            validate_web_url(referer)?;
        }
        // The page the cookies were collected on is source context only: it authorizes
        // partitioned cookies and never reaches the child as a Referer.
        validate_scoped_cookies(cookies, page_url)?;

        let mut inputs = Self {
            config: Vec::new(),
            redactions: vec![url.to_string()],
            cookie_jar: None,
        };
        for (name, value) in headers {
            writeln!(inputs.config, "--add-headers")?;
            writeln!(
                inputs.config,
                "{}",
                config_quote(&format!("{name}: {value}"))
            )?;
            inputs.redactions.push(format!("{name}: {value}"));
            if value.len() >= 4 {
                inputs.redactions.push(value.clone());
            }
        }
        if !cookies.is_empty() {
            let jar = write_cookie_jar(cookies)?;
            writeln!(inputs.config, "--cookies")?;
            writeln!(inputs.config, "{}", config_quote(&jar.to_string_lossy()))?;
            inputs.cookie_jar = Some(jar);
            inputs.redactions.extend(
                cookies
                    .iter()
                    .filter(|cookie| cookie.value.len() >= 4)
                    .map(|cookie| cookie.value.clone()),
            );
        }
        if let Some(referer) = referer {
            writeln!(inputs.config, "--referer")?;
            // Preserve the observed request; path-sensitive hotlink checks need the full Referer.
            writeln!(inputs.config, "{}", config_quote(referer))?;
            inputs.redactions.push(referer.to_string());
        }
        if include_url {
            writeln!(inputs.config, "{}", config_quote(url))?;
        }
        Ok(inputs)
    }
}

/// The page a request's cookies were observed on. Source context, never an HTTP Referer: it
/// only names the top-level site a partitioned cookie was collected under.
pub(super) fn page_context(value: &str) -> Result<Url> {
    if value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
        bail!("Kaynak sayfa bağlamı geçersiz satır karakteri içeriyor");
    }
    let parsed = Url::parse(value).context("Kaynak sayfa bağlamı tam bir kaynak adresi değil")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("Kaynak sayfa bağlamı geçerli bir web adresi değil");
    }
    Ok(parsed)
}

pub(super) fn validate_scoped_cookies(
    cookies: &[ScopedCookie],
    page_url: Option<&str>,
) -> Result<()> {
    if cookies.len() > 500 {
        bail!("Seçili kaynak zincirinin çerez sınırı aşıldı");
    }
    // An empty page is the same as no page: only a partitioned cookie needs it, and a
    // supplied page is validated before it authorizes one. The network Referer is a
    // different observation and never stands in.
    let page_url = page_url.filter(|value| !value.is_empty());
    let page = page_url
        .filter(|_| cookies.iter().any(|cookie| cookie.partition_key.is_some()))
        .map(page_context)
        .transpose()?;
    let mut store: Option<Option<&str>> = None;
    let mut partition: Option<&str> = None;
    let mut netscape_identities = std::collections::BTreeMap::new();
    for cookie in cookies {
        if cookie.name.is_empty()
            || cookie
                .name
                .bytes()
                .any(|byte| byte <= b' ' || matches!(byte, b';' | b',' | b'=' | 0x7f))
            || cookie
                .value
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n' | b'\t' | b';'))
            || cookie
                .domain
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n' | b'\t'))
            || cookie
                .path
                .bytes()
                .any(|byte| matches!(byte, 0 | b'\r' | b'\n' | b'\t'))
        {
            bail!("Oturum çerezi güvenli Netscape dosyasına dönüştürülemedi");
        }
        let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
        let parsed_domain = Url::parse(&format!("https://{domain}/"))
            .context("Oturum çerezi sunucu kapsamı geçersiz")?;
        if domain.is_empty()
            || parsed_domain.host_str() != Some(domain.as_str())
            || parsed_domain.port().is_some()
        {
            bail!("Oturum çerezi sunucu kapsamı geçersiz");
        }
        if !cookie.path.starts_with('/') {
            bail!("Oturum çerezi yol kapsamı geçersiz");
        }

        let store_id = cookie.store_id.as_deref();
        if store_id.is_some_and(str::is_empty) {
            bail!("Oturum çerezi depo kapsamı geçersiz");
        }
        match store {
            None => store = Some(store_id),
            Some(previous) if previous != store_id => {
                bail!("Farklı tarayıcı çerez depoları tek medya isteğinde birleştirilemez")
            }
            Some(_) => {}
        }

        let partition_key = cookie.partition_key.as_deref();
        if let Some(key) = partition_key {
            if partition.is_some_and(|previous| previous != key) {
                bail!("Bir medya isteğinde farklı partition çerezleri birleştirilemez");
            }
            let top =
                Url::parse(key).context("Çerez partition anahtarı tam bir kaynak adresi değil")?;
            if !matches!(top.scheme(), "http" | "https")
                || top.host_str().is_none()
                || top.username() != ""
                || top.password().is_some()
                || top.path() != "/"
                || top.query().is_some()
                || top.fragment().is_some()
            {
                bail!("Çerez partition anahtarı tam bir kaynak adresi değil");
            }
            let page = page
                .as_ref()
                .context("Partition çerezi için kaynak sayfa bağlamı eksik")?;
            let top_host = top.host_str().unwrap_or_default();
            let page_host = page.host_str().unwrap_or_default();
            if top.scheme() != page.scheme()
                || !(page_host.eq_ignore_ascii_case(top_host)
                    || page_host
                        .to_ascii_lowercase()
                        .ends_with(&format!(".{}", top_host.to_ascii_lowercase())))
            {
                bail!("Çerez partition anahtarı kaynak sayfa bağlamıyla eşleşmiyor");
            }
            partition = Some(key);
        }

        let identity = (domain, cookie.path.as_str(), cookie.name.as_str());
        if netscape_identities
            .insert(identity, partition_key)
            .is_some_and(|previous| previous != partition_key)
        {
            bail!("Aynı çerez için partition ve partitionsız değerler güvenle birleştirilemiyor");
        }
    }
    Ok(())
}

pub(super) fn scoped_cookie_applies(cookie: &ScopedCookie, target: &Url) -> bool {
    if cookie.secure && target.scheme() != "https" {
        return false;
    }
    if cookie.expires.is_some_and(|expires| {
        !expires.is_finite() || expires <= chrono::Utc::now().timestamp() as f64
    }) {
        return false;
    }
    let Some(host) = target.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
    if if cookie.host_only {
        host != domain
    } else {
        host != domain && !host.ends_with(&format!(".{domain}"))
    } {
        return false;
    }
    let request_path = target.path();
    let cookie_path = cookie.path.as_str();
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

/// Removes cookie jars left behind by a hard kill. Called once at engine start,
/// before any job can create a live jar; per-job creation never sweeps, so
/// concurrent media jobs keep their own jars.
pub(crate) fn sweep_stale_cookie_jars() {
    let Ok(dir) = crate::paths::cookie_jar_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("ssdownload-cookie-") && name.ends_with(".txt") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub(super) fn write_cookie_jar<'a>(
    cookies: impl IntoIterator<Item = &'a ScopedCookie>,
) -> Result<PathBuf> {
    let path = crate::paths::cookie_jar_dir()?
        .join(format!("ssdownload-cookie-{}.txt", uuid::Uuid::new_v4()));
    let mut guard = TemporaryCookieJar(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&guard.0)?;
    writeln!(file, "# Netscape HTTP Cookie File")?;
    for cookie in cookies {
        let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
        let domain = if cookie.host_only {
            domain
        } else {
            format!(".{domain}")
        };
        let domain = if cookie.http_only {
            format!("#HttpOnly_{domain}")
        } else {
            domain
        };
        let expires = cookie
            .expires
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.floor() as i64)
            .unwrap_or(0);
        writeln!(
            file,
            "{domain}\t{}\t{}\t{}\t{expires}\t{}\t{}",
            if cookie.host_only { "FALSE" } else { "TRUE" },
            cookie.path,
            if cookie.secure { "TRUE" } else { "FALSE" },
            cookie.name,
            cookie.value
        )?;
    }
    file.flush()?;
    Ok(std::mem::take(&mut guard.0))
}

pub(super) struct TemporaryCookieJar(pub(super) PathBuf);

impl Drop for TemporaryCookieJar {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn write_input(
    child: &mut Child,
    input: &[u8],
) -> Result<thread::JoinHandle<std::io::Result<()>>> {
    let mut stdin = child
        .stdin
        .take()
        .context("Medya standart girdisi açılamadı")?;
    let input = input.to_vec();
    Ok(thread::Builder::new()
        .name("media-input".into())
        .spawn(move || stdin.write_all(&input))?)
}

pub(super) fn validate_web_url(value: &str) -> Result<()> {
    if value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
        bail!("URL geçersiz satır karakteri içeriyor");
    }
    let parsed = Url::parse(value).context("Geçerli bir medya URL'si girin")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("Yalnızca HTTP/HTTPS medya adresleri destekleniyor");
    }
    Ok(())
}
pub(super) fn config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
