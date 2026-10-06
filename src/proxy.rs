//! Outbound proxy selection shared by every transfer.
//!
//! `Settings.proxy` picks `system` (the Windows static proxy of the current user),
//! `none` or `manual` (`http://` / `socks5://` with optional credentials). The resolved
//! choice is process-wide: libcurl handles apply it when they are created, and the media
//! tools' loopback proxy opens its outbound tunnels through it. PAC scripts are not
//! evaluated; a `system` profile that only has an automatic-configuration script connects
//! directly.

use crate::model::ProxySettings;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Upstream {
    Direct,
    Http {
        host: String,
        port: u16,
        credentials: Option<(String, String)>,
    },
    Socks5 {
        host: String,
        port: u16,
        credentials: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyConfig {
    upstream: Upstream,
    /// Host patterns reached directly (`<local>`, `*.corp.example`, `10.*`).
    bypass: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            upstream: Upstream::Direct,
            bypass: Vec::new(),
        }
    }
}

static CURRENT: RwLock<Option<Arc<ProxyConfig>>> = RwLock::new(None);

/// Resolves and installs the proxy selected by `settings`. An unusable selection (an
/// unreadable password, a malformed system entry) falls back to a direct connection and
/// is reported through the returned error so the caller can surface it.
pub(crate) fn configure(settings: &ProxySettings) -> Result<()> {
    let (config, result) = match resolve(settings) {
        Ok(config) => (config, Ok(())),
        Err(error) => (ProxyConfig::default(), Err(error)),
    };
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(config));
    result
}

pub(crate) fn current() -> Arc<ProxyConfig> {
    CURRENT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

fn resolve(settings: &ProxySettings) -> Result<ProxyConfig> {
    match settings.mode.as_str() {
        "none" => Ok(ProxyConfig::default()),
        "manual" => {
            let credentials = credentials(&settings.username, &settings.password)?;
            let url = url::Url::parse(settings.url.trim()).context("Proxy adresi geçersiz")?;
            let host = url.host_str().context("Proxy sunucusu eksik")?.to_owned();
            let port = url.port_or_known_default().context("Proxy portu eksik")?;
            let upstream = match url.scheme() {
                "http" => Upstream::Http {
                    host,
                    port,
                    credentials,
                },
                "socks5" | "socks5h" => Upstream::Socks5 {
                    host,
                    port,
                    credentials,
                },
                _ => bail!("Desteklenmeyen proxy türü"),
            };
            Ok(ProxyConfig {
                upstream,
                bypass: vec!["<local>".into()],
            })
        }
        _ => system_proxy(),
    }
}

fn credentials(username: &str, sealed: &str) -> Result<Option<(String, String)>> {
    if username.is_empty() {
        return Ok(None);
    }
    let password = if sealed.is_empty() {
        String::new()
    } else {
        crate::secure::unseal(sealed).context("Proxy parolası açılamadı")?
    };
    Ok(Some((username.to_owned(), password)))
}

/// The current user's WinINET static proxy (`ProxyEnable`, `ProxyServer`,
/// `ProxyOverride`).
fn system_proxy() -> Result<ProxyConfig> {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings");
    let Ok(key) = key else {
        return Ok(ProxyConfig::default());
    };
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return Ok(ProxyConfig::default());
    }
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    let bypass: String = key.get_value("ProxyOverride").unwrap_or_default();
    let Some((host, port)) = parse_system_server(&server) else {
        return Ok(ProxyConfig::default());
    };
    Ok(ProxyConfig {
        upstream: Upstream::Http {
            host,
            port,
            credentials: None,
        },
        bypass: bypass
            .split(';')
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty())
            .collect(),
    })
}

/// `host:port`, or the `https=` (then `http=`) entry of a per-protocol list.
fn parse_system_server(value: &str) -> Option<(String, u16)> {
    let value = value.trim();
    let entry = if value.contains('=') {
        let entries = value
            .split(';')
            .filter_map(|part| part.split_once('='))
            .map(|(scheme, target)| (scheme.trim().to_ascii_lowercase(), target.trim()))
            .collect::<Vec<_>>();
        ["https", "http"]
            .iter()
            .find_map(|wanted| {
                entries
                    .iter()
                    .find(|(scheme, _)| scheme == wanted)
                    .map(|(_, target)| *target)
            })?
            .to_owned()
    } else {
        value.to_owned()
    };
    let entry = entry
        .strip_prefix("http://")
        .unwrap_or(&entry)
        .trim_end_matches('/');
    let (host, port) = entry.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

impl ProxyConfig {
    /// The upstream for `host`, honouring the bypass list.
    pub(crate) fn upstream_for(&self, host: &str) -> &Upstream {
        if self.bypasses(host) {
            &Upstream::Direct
        } else {
            &self.upstream
        }
    }

    fn bypasses(&self, host: &str) -> bool {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        if host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            return true;
        }
        self.bypass.iter().any(|pattern| {
            if pattern == "<local>" {
                !host.contains('.') && !host.contains(':')
            } else {
                wildcard_match(pattern, &host)
            }
        })
    }

    /// Applies the proxy to a libcurl handle about to fetch from `host` (`None` when the
    /// host is not known yet). A direct choice sets an explicit empty proxy so libcurl
    /// never picks one up from the environment.
    pub(crate) fn apply(&self, easy: &mut curl::easy::Easy, host: Option<&str>, ftp: bool) {
        let upstream = match host {
            Some(host) => self.upstream_for(host),
            None => &self.upstream,
        };
        let _ = match upstream {
            Upstream::Direct => easy.proxy(""),
            // libcurl would translate FTP into HTTP requests through an HTTP proxy;
            // FTP stays direct there and only uses a SOCKS tunnel.
            Upstream::Http { .. } if ftp => easy.proxy(""),
            Upstream::Http {
                host,
                port,
                credentials,
            } => easy
                .proxy(&format!("http://{}:{port}", bracket(host)))
                .and_then(|_| apply_credentials(easy, credentials.as_ref())),
            Upstream::Socks5 {
                host,
                port,
                credentials,
            } => easy
                .proxy(&format!("socks5h://{}:{port}", bracket(host)))
                .and_then(|_| apply_credentials(easy, credentials.as_ref())),
        };
    }

    /// Opens a TCP tunnel to `host:port` through the configured upstream.
    pub(crate) fn connect(&self, host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
        match self.upstream_for(host) {
            Upstream::Direct => connect_direct(host, port, timeout),
            Upstream::Http {
                host: proxy_host,
                port: proxy_port,
                credentials,
            } => {
                let mut stream = connect_direct(proxy_host, *proxy_port, timeout)?;
                http_connect(&mut stream, host, port, credentials.as_ref(), timeout)?;
                Ok(stream)
            }
            Upstream::Socks5 {
                host: proxy_host,
                port: proxy_port,
                credentials,
            } => {
                let mut stream = connect_direct(proxy_host, *proxy_port, timeout)?;
                socks5_connect(&mut stream, host, port, credentials.as_ref(), timeout)?;
                Ok(stream)
            }
        }
    }
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn apply_credentials(
    easy: &mut curl::easy::Easy,
    credentials: Option<&(String, String)>,
) -> std::result::Result<(), curl::Error> {
    if let Some((username, password)) = credentials {
        easy.proxy_username(username)?;
        easy.proxy_password(password)?;
    }
    Ok(())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut rest = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(tail) = rest.strip_prefix(part) else {
                return false;
            };
            rest = tail;
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(found) = rest.find(part) else {
                return false;
            };
            rest = &rest[found + part.len()..];
        }
    }
    true
}

fn connect_direct(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addresses = (host, port)
        .to_socket_addrs()
        .context("Uzak sunucu adı çözümlenemedi")?
        .collect::<Vec<_>>();
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("Uzak sunucu adresi yok")))
}

fn http_connect(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&(String, String)>,
    timeout: Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    let authority = format!("{}:{port}", bracket(host));
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some((username, password)) = credentials {
        let token = STANDARD.encode(format!("{username}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            bail!("Proxy yanıt başlığı çok büyük");
        }
        if stream.read(&mut byte)? == 0 {
            bail!("Proxy bağlantıyı kapattı");
        }
        head.push(byte[0]);
    }
    stream.set_read_timeout(None)?;
    let status = std::str::from_utf8(&head)
        .ok()
        .and_then(|text| text.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    match status {
        200..=299 => Ok(()),
        407 => bail!("Proxy kimlik doğrulaması reddedildi (HTTP 407)"),
        other => bail!("Proxy tüneli açılamadı (HTTP {other})"),
    }
}

fn socks5_connect(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    credentials: Option<&(String, String)>,
    timeout: Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    let methods: &[u8] = if credentials.is_some() {
        &[0x00, 0x02]
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting)?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice)?;
    if choice[0] != 0x05 {
        bail!("SOCKS5 sunucusu geçersiz yanıt verdi");
    }
    match (choice[1], credentials) {
        (0x00, _) => {}
        (0x02, Some((username, password))) => {
            if username.len() > 255 || password.len() > 255 {
                bail!("SOCKS5 kimlik bilgileri çok uzun");
            }
            let mut auth = vec![0x01, username.len() as u8];
            auth.extend_from_slice(username.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status)?;
            if status[1] != 0x00 {
                bail!("SOCKS5 kimlik doğrulaması reddedildi");
            }
        }
        _ => bail!("SOCKS5 sunucusu desteklenen bir kimlik doğrulama yöntemi sunmadı"),
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let mut request = vec![0x05, 0x01, 0x00];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => {
            request.push(0x01);
            request.extend_from_slice(&address.octets());
        }
        Ok(std::net::IpAddr::V6(address)) => {
            request.push(0x04);
            request.extend_from_slice(&address.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                bail!("SOCKS5 hedef adı çok uzun");
            }
            request.push(0x03);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request)?;
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply)?;
    if reply[1] != 0x00 {
        bail!("SOCKS5 bağlantısı reddedildi (kod {})", reply[1]);
    }
    let skip = match reply[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length)?;
            usize::from(length[0])
        }
        _ => bail!("SOCKS5 yanıtı geçersiz"),
    };
    let mut bound = vec![0u8; skip + 2];
    stream.read_exact(&mut bound)?;
    stream.set_read_timeout(None)?;
    Ok(())
}

static SITE_LOGINS: RwLock<Vec<crate::model::SiteLogin>> = RwLock::new(Vec::new());

/// Installs the per-host HTTP Basic credentials used by file transfers.
pub(crate) fn configure_site_logins(logins: &[crate::model::SiteLogin]) {
    *SITE_LOGINS.write().unwrap_or_else(|e| e.into_inner()) = logins.to_vec();
}

/// The credentials for exactly this URL's host: HTTPS always, plain HTTP only when the
/// entry allows it. Another host, even a redirect target, never receives them.
pub(crate) fn site_login(url: &url::Url) -> Option<(String, String)> {
    let host = url.host_str()?.to_ascii_lowercase();
    let logins = SITE_LOGINS.read().unwrap_or_else(|e| e.into_inner());
    let login = logins
        .iter()
        .find(|login| login.host.trim().eq_ignore_ascii_case(&host))?;
    let allowed = match url.scheme() {
        "https" => true,
        "http" => login.allow_http,
        _ => false,
    };
    if !allowed {
        return None;
    }
    let password = if login.password.is_empty() {
        String::new()
    } else {
        crate::secure::unseal(&login.password).ok()?
    };
    Some((login.username.clone(), password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_proxy_entries_and_bypass_rules_resolve() {
        assert_eq!(
            parse_system_server("proxy.example:8080"),
            Some(("proxy.example".into(), 8080))
        );
        assert_eq!(
            parse_system_server("ftp=f.example:21;http=h.example:3128;https=s.example:3129"),
            Some(("s.example".into(), 3129))
        );
        assert_eq!(parse_system_server("socks=s.example:1080"), None);
        let config = ProxyConfig {
            upstream: Upstream::Http {
                host: "proxy.example".into(),
                port: 8080,
                credentials: None,
            },
            bypass: vec!["<local>".into(), "*.corp.example".into(), "10.*".into()],
        };
        assert_eq!(config.upstream_for("intranet"), &Upstream::Direct);
        assert_eq!(config.upstream_for("files.corp.example"), &Upstream::Direct);
        assert_eq!(config.upstream_for("10.1.2.3"), &Upstream::Direct);
        assert_eq!(config.upstream_for("127.0.0.1"), &Upstream::Direct);
        assert_ne!(config.upstream_for("cdn.example"), &Upstream::Direct);
    }
}
