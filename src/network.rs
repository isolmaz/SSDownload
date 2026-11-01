use crate::model::TransferControl;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use curl::easy::Easy;
use std::{
    collections::HashMap,
    ffi::c_void,
    io::{self, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{raw::c_int, windows::io::AsRawSocket},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use url::{Host, Url};
use uuid::Uuid;
use windows_sys::Win32::Networking::WinSock::{
    closesocket, connect, getsockopt, ioctlsocket, socket, WSAGetLastError, WSAPoll,
    WSASetLastError, AF_INET, AF_INET6, FIONBIO, INVALID_SOCKET, POLLERR, POLLHUP, POLLNVAL,
    POLLOUT, POLLRDNORM, SOCK_STREAM, SOL_SOCKET, SO_ERROR, WSAEALREADY, WSAECANCELLED,
    WSAEINPROGRESS, WSAEINVAL, WSAEISCONN, WSAETIMEDOUT, WSAEWOULDBLOCK, WSAPOLLFD,
};

pub(crate) const GLOBAL_CONNECTION_BUDGET: usize = 32;
/// How long a proxy tunnel waits for a slot before answering 503, polled in short steps. A
/// throttled source can hold a fragment's tunnel for many seconds, so the bound is generous
/// while staying far below the downloader's own connect timeout.
const TUNNEL_WAIT: Duration = Duration::from_secs(10);
/// Slots a metadata inspection may take above the per-host bound.
const INSPECTION_HEADROOM: usize = 2;
/// Slots a live recording may take above the per-host bound, see
/// `ExternalProxy::spawn_for_live_recording`.
const LIVE_RECORDING_HEADROOM: usize = 2;
const TUNNEL_WAIT_STEP: Duration = Duration::from_millis(50);
/// A tunnel whose last bytes were the server's and that has been silent this long gives its
/// slot back while it stays open, and takes a slot again before the client's next bytes are
/// forwarded. Pooled HTTP clients keep idle connections open per host; counted, they fill the
/// budgets while the next request is refused. The proxy cannot see inside TLS, so a server
/// that pauses mid-answer, or a request whose TLS 1.3 session ticket arrived after it, also
/// gives its slot back: that briefly admits one request more than the bound, it never cuts
/// a transfer.
const IDLE_SLOT_RELEASE_AFTER: Duration = Duration::from_secs(1);
const RELAY_BUFFER: usize = 64 * 1024;
const RELAY_POLL: Duration = Duration::from_millis(250);
const ACQUIRE_POLL: Duration = Duration::from_millis(50);
/// A request refused without waiting counts as waiting this long after the refusal. Such a
/// requester asks again on its own: a browser transfer asks for its permit again at most a
/// second later.
const REFUSAL_WAITS_FOR: Duration = Duration::from_secs(2);
const DEFAULT_CURL_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PROXY_HEAD_LIMIT: usize = 16 * 1024;
const PROXY_CLIENT_LIMIT: usize = 64;
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PROXY_HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const PROXY_RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(crate) struct NetworkGovernor {
    inner: Arc<GovernorInner>,
}

struct GovernorInner {
    state: Mutex<GovernorState>,
    available: Condvar,
}

struct GovernorState {
    per_host_limit: usize,
    active: usize,
    hosts: HashMap<String, usize>,
    wildcards: usize,
    /// Requesters waiting for a slot of each host, and for a wildcard slot, which waits on
    /// every host.
    waiting: HashMap<String, usize>,
    waiting_wildcards: usize,
    /// The last refusal of a request that does not wait, for each host and for a wildcard
    /// slot. A refusal drops the hosts refused longer than `REFUSAL_WAITS_FOR` ago.
    refused: HashMap<String, Instant>,
    refused_wildcard: Option<Instant>,
}

/// Counts a requester as waiting for its scope until it is dropped.
struct Waiter<'a> {
    governor: &'a NetworkGovernor,
    /// `None` for a wildcard.
    host: Option<String>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut state = self.governor.lock_state();
        match self.host.take() {
            Some(host) => {
                if let Some(count) = state.waiting.get_mut(&host) {
                    *count -= 1;
                    if *count == 0 {
                        state.waiting.remove(&host);
                    }
                }
            }
            None => state.waiting_wildcards -= 1,
        }
    }
}

#[derive(Debug)]
enum LeaseScope {
    Host(String),
    Wildcard,
}

/// A counted network capacity unit. It is deliberately non-cloneable: a permit
/// follows the socket or explicit FTP reservation that owns it until that owner
/// has closed.
#[derive(Debug)]
pub(crate) struct ConnectionLease {
    governor: NetworkGovernor,
    scope: Option<LeaseScope>,
    units: usize,
}

impl std::fmt::Debug for NetworkGovernor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock_state();
        formatter
            .debug_struct("NetworkGovernor")
            .field("active", &state.active)
            .field("per_host_limit", &state.per_host_limit)
            .field("wildcards", &state.wildcards)
            .finish()
    }
}

impl NetworkGovernor {
    pub(crate) fn new(per_host_limit: u8) -> Self {
        Self {
            inner: Arc::new(GovernorInner {
                state: Mutex::new(GovernorState {
                    per_host_limit: usize::from(per_host_limit.max(1)),
                    active: 0,
                    hosts: HashMap::new(),
                    wildcards: 0,
                    waiting: HashMap::new(),
                    waiting_wildcards: 0,
                    refused: HashMap::new(),
                    refused_wildcard: None,
                }),
                available: Condvar::new(),
            }),
        }
    }

    pub(crate) fn set_per_host_limit(&self, per_host_limit: u8) {
        let mut state = self.lock_state();
        state.per_host_limit = usize::from(per_host_limit.max(1));
        self.inner.available.notify_all();
    }

    /// A lowered host cap applies immediately to new sockets. Existing actual
    /// sockets are never lied about or force-released; callers can surface this
    /// state until their real close callbacks drain below the new bound.
    pub(crate) fn per_host_limit_pending(&self) -> bool {
        let state = self.lock_state();
        state.wildcards > state.per_host_limit
            || state
                .hosts
                .values()
                .any(|count| count.saturating_add(state.wildcards) > state.per_host_limit)
    }

    /// Current per-host tunnel bound. Media jobs clamp their fragment
    /// parallelism to this so a child never requests tunnels the governor
    /// must reject with 503.
    pub(crate) fn per_host_limit(&self) -> u8 {
        self.lock_state().per_host_limit.min(u8::MAX as usize) as u8
    }

    pub(crate) fn try_acquire_host(&self, host: String) -> Option<ConnectionLease> {
        self.try_acquire(LeaseScope::Host(host), 1)
    }

    /// Whether a requester waits for a slot of `host`, or was refused one without waiting in
    /// the last `REFUSAL_WAITS_FOR`: a connection kept open between requests to it should then
    /// give its slot up. A requester counts for every host when it is after a wildcard slot,
    /// and while the global budget has no room for the largest request (an FTP reservation of
    /// two slots).
    pub(crate) fn host_has_waiters(&self, host: &str) -> bool {
        let state = self.lock_state();
        let now = Instant::now();
        let recent = |at: &Instant| now.saturating_duration_since(*at) < REFUSAL_WAITS_FOR;
        let wildcard =
            state.waiting_wildcards > 0 || state.refused_wildcard.as_ref().is_some_and(recent);
        let anyone = !state.waiting.is_empty() || state.refused.values().any(recent);
        wildcard
            || state.waiting.contains_key(host)
            || state.refused.get(host).is_some_and(recent)
            || (anyone && state.active + 2 > GLOBAL_CONNECTION_BUDGET)
    }

    /// Counts a requester of `scope` as waiting, while the caller holds the state lock.
    fn waiter(&self, state: &mut GovernorState, scope: &LeaseScope) -> Waiter<'_> {
        let host = match scope {
            LeaseScope::Host(host) => {
                *state.waiting.entry(host.clone()).or_insert(0) += 1;
                Some(host.clone())
            }
            LeaseScope::Wildcard => {
                state.waiting_wildcards += 1;
                None
            }
        };
        Waiter {
            governor: self,
            host,
        }
    }

    /// Admission for one proxy tunnel: wait briefly for a finished fragment to release its slot
    /// before answering the client with a retryable 503. The wait is bounded, so a child that
    /// holds every slot of a host still gets its refusal instead of deadlocking the proxy.
    pub(crate) fn acquire_host_for_tunnel(
        &self,
        host: String,
        timeout: Duration,
        headroom: usize,
    ) -> Option<ConnectionLease> {
        let deadline = Instant::now() + timeout;
        let mut waiter = None;
        loop {
            if let Some(lease) = self.try_acquire_host_with_headroom(host.clone(), headroom) {
                return Some(lease);
            }
            if Instant::now() >= deadline {
                return None;
            }
            let mut state = self.lock_state();
            waiter.get_or_insert_with(|| self.waiter(&mut state, &LeaseScope::Host(host.clone())));
            let (next, _) = self
                .inner
                .available
                .wait_timeout(state, TUNNEL_WAIT_STEP)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drop(next);
        }
    }

    /// A host permit that may exceed the per-host bound by `headroom` slots. The permit
    /// still counts at its host, so downloads see it; only the admission is wider.
    fn try_acquire_host_with_headroom(
        &self,
        host: String,
        headroom: usize,
    ) -> Option<ConnectionLease> {
        let mut state = self.lock_state();
        let count = state.hosts.get(&host).copied().unwrap_or(0);
        let admitted = state.active < GLOBAL_CONNECTION_BUDGET
            && count.saturating_add(state.wildcards)
                < state.per_host_limit.saturating_add(headroom);
        if !admitted {
            return None;
        }
        let scope = LeaseScope::Host(host);
        grant(&mut state, &scope, 1);
        Some(ConnectionLease {
            governor: self.clone(),
            scope: Some(scope),
            units: 1,
        })
    }

    /// `host=<n> wildcards=<n> active=<n> limit=<n>` for a refused admission's log line.
    pub(crate) fn usage_summary(&self, host: &str) -> String {
        let state = self.lock_state();
        format!(
            "host={} wildcards={} active={} limit={}",
            state.hosts.get(host).copied().unwrap_or(0),
            state.wildcards,
            state.active,
            state.per_host_limit
        )
    }

    pub(crate) fn try_acquire_wildcard(&self) -> Option<ConnectionLease> {
        self.try_acquire(LeaseScope::Wildcard, 1)
    }

    /// Waits only before an Easy owns its first socket. This runs in a transfer
    /// worker, never the actor, and checks cancellation while other independent
    /// handles make progress and release capacity.
    fn acquire_host(&self, host: String, control: &TransferControl) -> Result<ConnectionLease> {
        self.acquire(LeaseScope::Host(host), control, None)
    }

    fn acquire_wildcard(&self, control: &TransferControl) -> Result<ConnectionLease> {
        self.acquire(LeaseScope::Wildcard, control, None)
    }

    /// Waits for capacity until `deadline`, when there is one.
    fn acquire(
        &self,
        scope: LeaseScope,
        control: &TransferControl,
        deadline: Option<Instant>,
    ) -> Result<ConnectionLease> {
        let mut waiter = None;
        loop {
            if control.stop_requested() {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Ağ bağlantısı kapasite beklerken durduruldu",
                        "The network connection was stopped while waiting for capacity"
                    )
                );
            }
            let mut state = self.lock_state();
            if can_acquire(&state, &scope, 1) {
                grant(&mut state, &scope, 1);
                return Ok(ConnectionLease {
                    governor: self.clone(),
                    scope: Some(scope),
                    units: 1,
                });
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Ağ bağlantısı için zamanında kapasite açılmadı",
                        "No capacity for the network connection became free in time"
                    )
                );
            }
            waiter.get_or_insert_with(|| self.waiter(&mut state, &scope));
            let (next, _) = self
                .inner
                .available
                .wait_timeout(state, ACQUIRE_POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drop(next);
        }
    }

    /// FTP needs its control and data sockets concurrently. Reserving both slots
    /// before libcurl opens control prevents two FTP transfers from each holding a
    /// control socket forever while waiting for their data sockets. The reservation
    /// is conservative and is released only after the easy handle has closed.
    pub(crate) fn reserve_ftp(
        &self,
        host: String,
        control: &TransferControl,
    ) -> Result<ConnectionLease> {
        let mut waiter = None;
        loop {
            if control.stop_requested() {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "FTP bağlantısı kapasite beklerken durduruldu",
                        "The FTP connection was stopped while waiting for capacity"
                    )
                );
            }
            let mut state = self.lock_state();
            if state.per_host_limit < 2 {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "FTP aktarımı aynı anda denetim ve veri soketi gerektirir; sunucu başına bağlantı sınırı en az 2 olmalıdır",
                        "An FTP transfer needs a control and a data socket at the same time; the per-server connection limit must be at least 2"
                    )
                );
            }
            let scope = LeaseScope::Host(host.clone());
            if can_acquire(&state, &scope, 2) {
                grant(&mut state, &scope, 2);
                return Ok(ConnectionLease {
                    governor: self.clone(),
                    scope: Some(scope),
                    units: 2,
                });
            }
            waiter.get_or_insert_with(|| self.waiter(&mut state, &scope));
            let (next, _) = self
                .inner
                .available
                .wait_timeout(state, ACQUIRE_POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drop(next);
        }
    }

    pub(crate) fn easy_for_url(
        &self,
        url: &Url,
        control: &TransferControl,
    ) -> Result<GovernedEasy> {
        self.easy_for_url_by(url, control, None)
    }

    /// Like `easy_for_url`, but fails once `deadline` has passed while the host has no free slot.
    pub(crate) fn easy_for_url_before(
        &self,
        url: &Url,
        control: &TransferControl,
        deadline: Instant,
    ) -> Result<GovernedEasy> {
        self.easy_for_url_by(url, control, Some(deadline))
    }

    fn easy_for_url_by(
        &self,
        url: &Url,
        control: &TransferControl,
        deadline: Option<Instant>,
    ) -> Result<GovernedEasy> {
        let host = canonical_host(url)?;
        // Acquire before libcurl starts its connect/Happy-Eyeballs timers.
        // Waiting inside the first open-socket callback can make a later
        // alternate-address attempt see the first socket's sole host permit.
        let first_socket = self.acquire(LeaseScope::Host(host.clone()), control, deadline)?;
        let mut easy = GovernedEasy::new(
            self.clone(),
            CurlScope::Host(host.clone()),
            control.clone(),
            Some(first_socket),
        )?;
        crate::proxy::current().apply(&mut easy, Some(&host), false);
        Ok(easy)
    }

    /// Automatic curl redirects have no safe hostname hand-off point before the
    /// next socket callback. A wildcard slot conservatively counts against every
    /// currently known host, so an undisclosed redirect cannot bypass a host cap.
    pub(crate) fn wildcard_easy(&self, control: &TransferControl) -> Result<GovernedEasy> {
        let first_socket = self.acquire_wildcard(control)?;
        let mut easy = GovernedEasy::new(
            self.clone(),
            CurlScope::Wildcard,
            control.clone(),
            Some(first_socket),
        )?;
        crate::proxy::current().apply(&mut easy, None, false);
        Ok(easy)
    }

    pub(crate) fn ftp_easy(&self, url: &Url, control: &TransferControl) -> Result<GovernedEasy> {
        let host = canonical_host(url)?;
        let reservation = self.reserve_ftp(host.clone(), control)?;
        let mut easy = GovernedEasy::new(self.clone(), CurlScope::Ftp, control.clone(), None)
            .map(|easy| easy.with_ftp_reservation(reservation))?;
        crate::proxy::current().apply(&mut easy, Some(&host), true);
        Ok(easy)
    }

    fn try_acquire(&self, scope: LeaseScope, units: usize) -> Option<ConnectionLease> {
        let mut state = self.lock_state();
        if !can_acquire(&state, &scope, units) {
            let now = Instant::now();
            state
                .refused
                .retain(|_, at| now.saturating_duration_since(*at) < REFUSAL_WAITS_FOR);
            match scope {
                LeaseScope::Host(host) => {
                    state.refused.insert(host, now);
                }
                LeaseScope::Wildcard => state.refused_wildcard = Some(now),
            }
            return None;
        }
        grant(&mut state, &scope, units);
        Some(ConnectionLease {
            governor: self.clone(),
            scope: Some(scope),
            units,
        })
    }

    fn release(&self, scope: LeaseScope, units: usize) {
        let mut state = self.lock_state();
        match scope {
            LeaseScope::Host(host) => {
                let remove = if let Some(count) = state.hosts.get_mut(&host) {
                    *count = count.saturating_sub(units);
                    *count == 0
                } else {
                    false
                };
                if remove {
                    state.hosts.remove(&host);
                }
            }
            LeaseScope::Wildcard => {
                state.wildcards = state.wildcards.saturating_sub(units);
            }
        }
        state.active = state.active.saturating_sub(units);
        self.inner.available.notify_all();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, GovernorState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.take() {
            self.governor.release(scope, self.units);
        }
    }
}

fn can_acquire(state: &GovernorState, scope: &LeaseScope, units: usize) -> bool {
    if units == 0 || state.active.saturating_add(units) > GLOBAL_CONNECTION_BUDGET {
        return false;
    }
    match scope {
        LeaseScope::Host(host) => {
            state
                .hosts
                .get(host)
                .copied()
                .unwrap_or(0)
                .saturating_add(state.wildcards)
                .saturating_add(units)
                <= state.per_host_limit
        }
        LeaseScope::Wildcard => {
            // A wildcard is conservatively present at every host, including a
            // host not yet observed. The independent check is therefore needed
            // even while `hosts` is empty.
            units == 1
                && state.wildcards.saturating_add(units) <= state.per_host_limit
                && state.hosts.values().all(|count| {
                    count.saturating_add(state.wildcards).saturating_add(units)
                        <= state.per_host_limit
                })
        }
    }
}

fn grant(state: &mut GovernorState, scope: &LeaseScope, units: usize) {
    state.active = state.active.saturating_add(units);
    match scope {
        LeaseScope::Host(host) => {
            *state.hosts.entry(host.clone()).or_insert(0) += units;
        }
        LeaseScope::Wildcard => state.wildcards = state.wildcards.saturating_add(units),
    }
}

pub(crate) fn canonical_host(url: &Url) -> Result<String> {
    // Do not derive identity from `host_str()`: its IPv6 serialization includes
    // brackets, while a DNS name needs trailing-dot normalization. `Url` has
    // already applied IDNA and parsed address literals structurally.
    url.host()
        .and_then(canonical_host_form)
        .context(crate::i18n::ui(
            "Ağ adresinde sunucu adı yok",
            "The network address has no host name",
        ))
}

/// The canonical form of a host name typed in the settings: `bücher.example`,
/// `BÜCHER.example` and `xn--bcher-kva.example.` all become `xn--bcher-kva.example`.
pub(crate) fn canonical_host_name(name: &str) -> Option<String> {
    canonical_host_form(Host::parse(name.trim()).ok()?)
}

fn canonical_host_form<S: AsRef<str>>(host: Host<S>) -> Option<String> {
    match host {
        Host::Domain(domain) => {
            let host = domain.as_ref().trim_end_matches('.').to_ascii_lowercase();
            (!host.is_empty()).then_some(host)
        }
        Host::Ipv4(address) => Some(address.to_string()),
        Host::Ipv6(address) => Some(address.to_string()),
    }
}

#[derive(Clone)]
enum CurlScope {
    Host(String),
    Wildcard,
    Ftp,
}

struct SocketEntry {
    lease: Option<ConnectionLease>,
    preconnected: bool,
}

impl SocketEntry {
    fn lease(lease: ConnectionLease) -> Self {
        Self {
            lease: Some(lease),
            preconnected: false,
        }
    }

    fn ftp_reservation() -> Self {
        Self {
            lease: None,
            preconnected: false,
        }
    }
}

struct CurlSocketState {
    governor: NetworkGovernor,
    scope: CurlScope,
    control: TransferControl,
    connect_timeout_ms: AtomicU64,
    // The first permit is acquired before libcurl starts its timers, then
    // transferred to the first actual socket opened by this Easy.
    primed: Mutex<Option<ConnectionLease>>,
    sockets: Mutex<HashMap<curl_sys::curl_socket_t, SocketEntry>>,
    failure: Mutex<Option<String>>,
}

struct CurlSocketCallbacks {
    state: Arc<CurlSocketState>,
}

/// An `Easy` whose socket callbacks are installed for its complete lifetime.
/// Its drop order is intentional: libcurl closes every socket while callback
/// state and its leases are still alive, then any defensively retained mapping
/// is released after `Easy` itself is gone.
pub(crate) struct GovernedEasy {
    easy: Option<Easy>,
    callbacks: CurlSocketCallbacks,
    ftp_reservation: Option<ConnectionLease>,
}

impl GovernedEasy {
    fn new(
        governor: NetworkGovernor,
        scope: CurlScope,
        control: TransferControl,
        first_socket: Option<ConnectionLease>,
    ) -> Result<Self> {
        let state = Arc::new(CurlSocketState {
            governor,
            scope,
            control,
            connect_timeout_ms: AtomicU64::new(DEFAULT_CURL_CONNECT_TIMEOUT.as_millis() as u64),
            primed: Mutex::new(first_socket),
            sockets: Mutex::new(HashMap::new()),
            failure: Mutex::new(None),
        });
        let callbacks = CurlSocketCallbacks { state };
        let mut easy = Easy::new();
        callbacks.install(&mut easy)?;
        Ok(Self {
            easy: Some(easy),
            callbacks,
            ftp_reservation: None,
        })
    }

    fn with_ftp_reservation(mut self, reservation: ConnectionLease) -> Self {
        self.ftp_reservation = Some(reservation);
        self
    }

    /// Keeps the pre-connect callback bounded by the same timeout callers set
    /// on libcurl before a transfer starts.
    pub(crate) fn connect_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::result::Result<(), curl::Error> {
        let milliseconds = timeout.as_millis().min(u64::MAX as u128) as u64;
        self.callbacks
            .state
            .connect_timeout_ms
            .store(milliseconds.max(1), Ordering::Release);
        self.easy_mut().connect_timeout(timeout)
    }

    pub(crate) fn network_error(&self) -> Option<String> {
        self.callbacks
            .state
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn easy(&self) -> &Easy {
        self.easy.as_ref().expect("governed curl handle dropped")
    }

    fn easy_mut(&mut self) -> &mut Easy {
        self.easy.as_mut().expect("governed curl handle dropped")
    }
}

impl std::ops::Deref for GovernedEasy {
    type Target = Easy;

    fn deref(&self) -> &Self::Target {
        self.easy()
    }
}

impl std::ops::DerefMut for GovernedEasy {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.easy_mut()
    }
}

impl Drop for GovernedEasy {
    fn drop(&mut self) {
        drop(self.easy.take());
        self.callbacks.release_unclosed();
        drop(self.ftp_reservation.take());
    }
}

impl CurlSocketCallbacks {
    fn install(&self, easy: &mut Easy) -> Result<()> {
        let data = Arc::as_ptr(&self.state) as *mut c_void;
        let open: curl_sys::curl_opensocket_callback = curl_open_socket;
        let close: CloseSocketCallback = curl_close_socket;
        let sockopt: SockOptCallback = curl_socket_options;
        for (option, value) in [
            (
                curl_sys::CURLOPT_OPENSOCKETFUNCTION,
                open as *const () as *const c_void,
            ),
            (
                curl_sys::CURLOPT_CLOSESOCKETFUNCTION,
                close as *const () as *const c_void,
            ),
            (
                curl_sys::CURLOPT_SOCKOPTFUNCTION,
                sockopt as *const () as *const c_void,
            ),
        ] {
            let code = unsafe { curl_sys::curl_easy_setopt(easy.raw(), option, value) };
            if code != curl_sys::CURLE_OK {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "libcurl soket yaşam döngüsü aracılığı etkinleştirilemedi",
                        "The libcurl socket lifecycle hook could not be enabled"
                    )
                );
            }
        }
        for option in [
            curl_sys::CURLOPT_OPENSOCKETDATA,
            curl_sys::CURLOPT_CLOSESOCKETDATA,
            curl_sys::CURLOPT_SOCKOPTDATA,
        ] {
            let code = unsafe { curl_sys::curl_easy_setopt(easy.raw(), option, data) };
            if code != curl_sys::CURLE_OK {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "libcurl soket yaşam döngüsü verisi ayarlanamadı",
                        "The libcurl socket lifecycle data could not be set"
                    )
                );
            }
        }
        Ok(())
    }

    fn release_unclosed(&self) {
        let mut sockets = self
            .state
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for mut entry in sockets.drain().map(|(_, entry)| entry) {
            drop(entry.lease.take());
        }
        drop(sockets);
        drop(
            self.state
                .primed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
    }
}

type CloseSocketCallback = extern "C" fn(*mut c_void, curl_sys::curl_socket_t) -> c_int;
type SockOptCallback =
    extern "C" fn(*mut c_void, curl_sys::curl_socket_t, curl_sys::curlsocktype) -> c_int;

// curl-sys exposes the option but not these C callback return constants.
const CURL_SOCKOPT_OK: c_int = 0;
const CURL_SOCKOPT_ERROR: c_int = 1;
const CURL_SOCKOPT_ALREADY_CONNECTED: c_int = 2;

extern "C" fn curl_open_socket(
    data: *mut c_void,
    purpose: curl_sys::curlsocktype,
    address: *mut curl_sys::curl_sockaddr,
) -> curl_sys::curl_socket_t {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if data.is_null() || address.is_null() {
            return curl_sys::CURL_SOCKET_BAD;
        }
        let state = &*(data as *const CurlSocketState);
        if purpose != curl_sys::CURLSOCKTYPE_IPCXN {
            state.fail(crate::i18n::ui(
                "libcurl desteklenmeyen bir soket türü istedi",
                "libcurl requested an unsupported socket type",
            ));
            return curl_sys::CURL_SOCKET_BAD;
        }
        let family = (*address).family;
        let socktype = (*address).socktype;
        if !matches!(family, value if value == AF_INET as i32 || value == AF_INET6 as i32)
            || socktype != SOCK_STREAM
        {
            state.fail(crate::i18n::ui(
                "libcurl desteklenmeyen ağ soketi türü istedi",
                "libcurl requested an unsupported network socket type",
            ));
            return curl_sys::CURL_SOCKET_BAD;
        }

        // Connect before transferring descriptor ownership to libcurl. Its
        // Happy Eyeballs filter otherwise starts another address while the
        // first one is still pending; a host limit of one must serialize those
        // actual sockets rather than reject a valid fallback.
        let entry = state.try_open_entry();
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                state.fail(&crate::i18n::ui_format!(
                    "Ağ bağlantısı açılamadı: {error:#}",
                    "The network connection could not be opened: {error:#}",
                    error = error
                ));
                return curl_sys::CURL_SOCKET_BAD;
            }
        };

        let socket = socket(family, socktype, (*address).protocol);
        if socket == INVALID_SOCKET {
            state.fail(crate::i18n::ui(
                "Ağ soketi açılamadı",
                "The network socket could not be opened",
            ));
            drop(entry);
            return curl_sys::CURL_SOCKET_BAD;
        }
        let socket = socket as curl_sys::curl_socket_t;
        if state.preconnects_http() {
            match state.connect_before_curl(socket, address) {
                Ok(()) => entry.preconnected = true,
                Err(error) => {
                    // libcurl has not received this descriptor, so this close
                    // cannot race its close callback or a descriptor reuse.
                    let _ = closesocket(socket as _);
                    drop(entry);
                    WSASetLastError(error);
                    if state.control.stop_requested() {
                        state.fail(crate::i18n::ui(
                            "Ağ bağlantısı açılırken durduruldu",
                            "The network connection was stopped while opening",
                        ));
                    }
                    return curl_sys::CURL_SOCKET_BAD;
                }
            }
        }
        let mut sockets = state
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if sockets.contains_key(&socket) {
            // Do not replace the old entry: if libcurl ever reports a reused
            // descriptor before its close callback, replacing it would release
            // the old lease while its lifetime is still uncertain.
            drop(sockets);
            let _ = closesocket(socket as _);
            drop(entry);
            state.fail(crate::i18n::ui(
                "libcurl soket tanıtıcısını yeniden kullandı",
                "libcurl reused a socket handle",
            ));
            return curl_sys::CURL_SOCKET_BAD;
        }
        sockets.insert(socket, entry);
        socket
    }))
    .unwrap_or(curl_sys::CURL_SOCKET_BAD)
}

extern "C" fn curl_socket_options(
    data: *mut c_void,
    socket: curl_sys::curl_socket_t,
    purpose: curl_sys::curlsocktype,
) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if data.is_null() {
            return CURL_SOCKOPT_ERROR;
        }
        let state = &*(data as *const CurlSocketState);
        if purpose != curl_sys::CURLSOCKTYPE_IPCXN {
            state.fail(crate::i18n::ui(
                "libcurl desteklenmeyen soket seçeneği istedi",
                "libcurl requested an unsupported socket option",
            ));
            return CURL_SOCKOPT_ERROR;
        }
        let preconnected = state
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&socket)
            .map(|entry| entry.preconnected);
        match preconnected {
            Some(true) => CURL_SOCKOPT_ALREADY_CONNECTED,
            Some(false) => CURL_SOCKOPT_OK,
            None => {
                state.fail(crate::i18n::ui(
                    "libcurl kayıtsız soket seçeneği istedi",
                    "libcurl requested an unregistered socket option",
                ));
                CURL_SOCKOPT_ERROR
            }
        }
    }))
    .unwrap_or(CURL_SOCKOPT_ERROR)
}

extern "C" fn curl_close_socket(data: *mut c_void, socket: curl_sys::curl_socket_t) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let close_result = closesocket(socket as _);
        if !data.is_null() {
            let state = &*(data as *const CurlSocketState);
            let entry = state
                .sockets
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&socket);
            if let Some(mut entry) = entry {
                drop(entry.lease.take());
            }
        }
        close_result
    }))
    .unwrap_or(-1)
}

impl CurlSocketState {
    fn preconnects_http(&self) -> bool {
        !matches!(&self.scope, CurlScope::Ftp)
    }

    /// Establishes one address candidate while this callback still owns the
    /// descriptor. A failed candidate is closed before returning
    /// `CURL_SOCKET_BAD`, letting libcurl advance DNS without two live sockets
    /// for the same governed host.
    unsafe fn connect_before_curl(
        &self,
        socket: curl_sys::curl_socket_t,
        address: *const curl_sys::curl_sockaddr,
    ) -> std::result::Result<(), c_int> {
        let mut nonblocking = 1u32;
        if ioctlsocket(socket as _, FIONBIO, &mut nonblocking) != 0 {
            return Err(WSAGetLastError());
        }
        let address = &*address;
        if address.addrlen > i32::MAX as u32 {
            return Err(WSAEINVAL);
        }
        if connect(
            socket as _,
            std::ptr::addr_of!(address.addr).cast(),
            address.addrlen as i32,
        ) == 0
        {
            return Ok(());
        }
        let error = WSAGetLastError();
        if error == WSAEISCONN {
            return Ok(());
        }
        if !matches!(error, WSAEWOULDBLOCK | WSAEINPROGRESS | WSAEALREADY) {
            return Err(error);
        }

        let timeout = Duration::from_millis(self.connect_timeout_ms.load(Ordering::Acquire));
        let started = std::time::Instant::now();
        loop {
            if self.control.stop_requested() {
                return Err(WSAECANCELLED);
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(WSAETIMEDOUT);
            }
            let wait = timeout
                .saturating_sub(elapsed)
                .min(ACQUIRE_POLL)
                .as_millis()
                .max(1)
                .min(i32::MAX as u128) as i32;
            let mut poll = WSAPOLLFD {
                fd: socket as _,
                events: POLLOUT,
                revents: 0,
            };
            let ready = WSAPoll(&mut poll, 1, wait);
            if ready < 0 {
                return Err(WSAGetLastError());
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (POLLOUT | POLLERR | POLLHUP | POLLNVAL) == 0 {
                continue;
            }
            let mut socket_error = 0i32;
            let mut length = std::mem::size_of::<i32>() as i32;
            if self.control.stop_requested() {
                return Err(WSAECANCELLED);
            }
            if getsockopt(
                socket as _,
                SOL_SOCKET,
                SO_ERROR,
                (&mut socket_error as *mut i32).cast(),
                &mut length,
            ) != 0
            {
                return Err(WSAGetLastError());
            }
            return if socket_error == 0 {
                Ok(())
            } else {
                Err(socket_error)
            };
        }
    }

    fn try_open_entry(&self) -> Result<SocketEntry> {
        if self.control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Ağ bağlantısı açılmadan durduruldu",
                    "The network connection was stopped before opening"
                )
            );
        }
        if let Some(lease) = self
            .primed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return Ok(SocketEntry::lease(lease));
        }
        let owns_socket = !self
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty();
        let entry = match &self.scope {
            CurlScope::Host(host) if owns_socket => self
                .governor
                .try_acquire_host(host.clone())
                .map(SocketEntry::lease)
                .context(crate::i18n::ui(
                    "Aynı libcurl aktarımı ek bağlantı kapasitesini bekleyemez",
                    "The same libcurl transfer cannot wait for additional connection capacity",
                )),
            CurlScope::Host(host) => self
                .governor
                .acquire_host(host.clone(), &self.control)
                .map(SocketEntry::lease),
            CurlScope::Wildcard if owns_socket => self
                .governor
                .try_acquire_wildcard()
                .map(SocketEntry::lease)
                .context(crate::i18n::ui(
                    "Aynı libcurl aktarımı ek bağlantı kapasitesini bekleyemez",
                    "The same libcurl transfer cannot wait for additional connection capacity",
                )),
            CurlScope::Wildcard => self
                .governor
                .acquire_wildcard(&self.control)
                .map(SocketEntry::lease),
            CurlScope::Ftp => {
                let sockets = self
                    .sockets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if sockets.len() >= 2 {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "FTP iki eşzamanlı soket sınırını aştı",
                            "FTP exceeded the limit of two concurrent sockets"
                        )
                    );
                }
                Ok(SocketEntry::ftp_reservation())
            }
        };
        entry
    }

    fn fail(&self, message: &str) {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failure.is_none() {
            *failure = Some(message.to_string());
        }
    }
}

/// The time from `now` to `deadline` as a curl timeout, or `None` when less than a millisecond
/// is left. The curl crate hands libcurl whole milliseconds and libcurl reads 0 as "no timeout",
/// so a shorter remainder would let the request run without a limit instead of expiring.
pub(crate) fn time_left(deadline: Instant, now: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(now);
    (left >= Duration::from_millis(1)).then_some(left)
}

/// A private loopback HTTP CONNECT proxy for one child process. It does not
/// decrypt TLS and only creates an outbound socket after authenticating the
/// capability passed to that child. Every outbound socket owns a governor lease.
/// Epoch milliseconds shared by the proxy and the child liveness watchdog.
pub(crate) fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) struct ExternalProxy {
    shared: Arc<ProxyShared>,
    accept_thread: Option<JoinHandle<()>>,
    proxy_url: String,
}

struct ProxyShared {
    governor: NetworkGovernor,
    /// Extra per-host admission for this child (metadata inspection), see
    /// `NetworkGovernor::acquire_host_for_tunnel`.
    headroom: usize,
    control: TransferControl,
    capability: Vec<u8>,
    stopping: AtomicBool,
    active_clients: AtomicUsize,
    next_tunnel: AtomicU64,
    /// When the child last asked this proxy for an outbound connection. A fragment
    /// retry loop keeps attempting at most a backoff apart, so this stays fresh
    /// while the downloader itself is deliberately silent.
    last_outbound_at: Arc<AtomicU64>,
    tunnels: Mutex<HashMap<u64, TcpStream>>,
}

impl Drop for ProxyShared {
    fn drop(&mut self) {
        self.capability.fill(0);
    }
}

impl ExternalProxy {
    pub(crate) fn spawn(governor: NetworkGovernor, control: TransferControl) -> Result<Self> {
        Self::spawn_with_headroom(governor, control, 0)
    }

    /// A proxy for a short metadata inspection. Inspection opens only a few requests,
    /// so it is admitted `INSPECTION_HEADROOM` slots above the per-host bound: a running
    /// download from the same host, or browser permits, can no longer starve it.
    pub(crate) fn spawn_for_inspection(
        governor: NetworkGovernor,
        control: TransferControl,
    ) -> Result<Self> {
        Self::spawn_with_headroom(governor, control, INSPECTION_HEADROOM)
    }

    /// A proxy for a live stream recording. Its reader keeps a socket to the playlist it
    /// reloads and one or two to the segments, often on the same host, next to the tool's
    /// own kept-alive socket; within the bare per-host bound its next request is refused
    /// and the recording stalls. One stream is recorded at its own pace, so the few extra
    /// slots do not add download pressure on the host.
    pub(crate) fn spawn_for_live_recording(
        governor: NetworkGovernor,
        control: TransferControl,
    ) -> Result<Self> {
        Self::spawn_with_headroom(governor, control, LIVE_RECORDING_HEADROOM)
    }

    fn spawn_with_headroom(
        governor: NetworkGovernor,
        control: TransferControl,
        headroom: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).context(
            crate::i18n::ui(
                "Yerel medya aracısı başlatılamadı",
                "The local media proxy could not be started",
            ),
        )?;
        listener.set_nonblocking(true).context(crate::i18n::ui(
            "Yerel medya aracısı dinleme kipine alınamadı",
            "The local media proxy could not be set to listening mode",
        ))?;
        let address = listener.local_addr().context(crate::i18n::ui(
            "Yerel medya aracısı adresi okunamadı",
            "The local media proxy address could not be read",
        ))?;
        let capability = Uuid::new_v4().simple().to_string().into_bytes();
        let proxy_url = format!(
            "http://ssdownload:{}@{}:{}",
            String::from_utf8_lossy(&capability),
            address.ip(),
            address.port()
        );
        let shared = Arc::new(ProxyShared {
            governor,
            headroom,
            control,
            capability,
            stopping: AtomicBool::new(false),
            active_clients: AtomicUsize::new(0),
            next_tunnel: AtomicU64::new(1),
            last_outbound_at: Arc::new(AtomicU64::new(now_epoch_ms())),
            tunnels: Mutex::new(HashMap::new()),
        });
        let server = Arc::clone(&shared);
        let accept_thread = thread::Builder::new()
            .name("media-network-proxy".into())
            .spawn(move || proxy_accept_loop(listener, server))
            .context(crate::i18n::ui(
                "Yerel medya aracısı iş parçacığı başlatılamadı",
                "The local media proxy thread could not be started",
            ))?;
        Ok(Self {
            shared,
            accept_thread: Some(accept_thread),
            proxy_url,
        })
    }

    pub(crate) fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    /// Shared timestamp of the child's most recent outbound connection attempt.
    pub(crate) fn outbound_ping(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.shared.last_outbound_at)
    }

    pub(crate) fn apply_environment(&self, command: &mut Command) {
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "FTP_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "ftp_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            command.env_remove(name);
        }
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "FTP_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "ftp_proxy",
        ] {
            command.env(name, &self.proxy_url);
        }
    }

    /// yt-dlp reads this from its private stdin configuration rather than a
    /// persistent config file. Callers must redact the proxy URL from every
    /// captured diagnostic; the child environment also reaches FFmpeg and Deno
    /// descendants.
    pub(crate) fn append_ytdlp_configuration(&self, config: &mut Vec<u8>) -> Result<()> {
        let quote =
            |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
        writeln!(config, "--proxy")?;
        writeln!(config, "{}", quote(&self.proxy_url))?;
        // FFmpeg can be selected by yt-dlp for HLS/DASH. Its inherited proxy
        // environment is supplemented by the documented input option, so it
        // has no direct HTTP fallback path.
        writeln!(config, "--downloader-args")?;
        writeln!(
            config,
            "{}",
            quote(&format!("ffmpeg_i:-http_proxy {}", self.proxy_url))
        )?;
        Ok(())
    }

    pub(crate) fn stop(&self) {
        if !self.shared.stopping.swap(true, Ordering::AcqRel) {
            self.shared.close_tunnels();
        }
    }
}

impl Drop for ExternalProxy {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        // String guarantees UTF-8 here because the capability is ASCII. This
        // is the standard-library escape hatch needed to overwrite its backing
        // allocation before release.
        unsafe { self.proxy_url.as_mut_vec().fill(0) };
    }
}

fn proxy_accept_loop(listener: TcpListener, shared: Arc<ProxyShared>) {
    let mut workers = Vec::new();
    while !shared.stopping.load(Ordering::Acquire) && !shared.control.stop_requested() {
        match listener.accept() {
            Ok((stream, _)) => {
                // Winsock can inherit the listener's nonblocking state. Relay
                // uses blocking Read/Write plus timeouts, so explicitly reset
                // each accepted child socket before handing it to a worker.
                // Otherwise a transient WouldBlock is treated as EOF and can
                // make an HTTP client observe a curl 52 empty reply.
                if stream.set_nonblocking(false).is_err() {
                    let _ = write_proxy_error(
                        stream,
                        503,
                        crate::i18n::ui(
                            "Yerel ağ aracısı başlatılamadı",
                            "The local network proxy could not be started",
                        ),
                    );
                    continue;
                }
                if shared.active_clients.fetch_add(1, Ordering::AcqRel) >= PROXY_CLIENT_LIMIT {
                    shared.active_clients.fetch_sub(1, Ordering::AcqRel);
                    let _ = write_proxy_error(
                        stream,
                        503,
                        crate::i18n::ui("Yerel ağ aracısı dolu", "The local network proxy is full"),
                    );
                } else {
                    let worker_shared = Arc::clone(&shared);
                    match thread::Builder::new()
                        .name("media-network-tunnel".into())
                        .spawn(move || {
                            proxy_client(stream, worker_shared.clone());
                            worker_shared.active_clients.fetch_sub(1, Ordering::AcqRel);
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(_) => {
                            shared.active_clients.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let worker = workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
    }
    shared.stopping.store(true, Ordering::Release);
    shared.close_tunnels();
    for worker in workers {
        let _ = worker.join();
    }
}

/// The loopback proxy's capability credential is missing, duplicated or wrong. Answered
/// with 407 so a client that waits for the challenge retries with its credentials.
#[derive(Debug)]
struct ProxyAuthRejected(&'static str);

impl std::fmt::Display for ProxyAuthRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ProxyAuthRejected {}

fn proxy_client(mut client: TcpStream, shared: Arc<ProxyShared>) {
    let _ = client.set_read_timeout(Some(PROXY_HEAD_TIMEOUT));
    let parsed =
        match read_proxy_head(&mut client).and_then(|head| parse_proxy_request(&shared, &head)) {
            Ok(request) => request,
            Err(error) => {
                let status = if error.is::<ProxyAuthRejected>() {
                    407
                } else {
                    400
                };
                let _ = write_proxy_error(
                    client,
                    status,
                    crate::i18n::ui(
                        "Yerel ağ aracısı isteği reddedildi",
                        "The local network proxy rejected the request",
                    ),
                );
                return;
            }
        };
    let _ = client.set_read_timeout(None);
    if shared.stopping.load(Ordering::Acquire) || shared.control.stop_requested() {
        let _ = write_proxy_error(
            client,
            503,
            crate::i18n::ui(
                "Yerel ağ aracısı durduruldu",
                "The local network proxy was stopped",
            ),
        );
        return;
    }

    // A fragment that just finished releases its slot, so a saturated request waits briefly
    // instead of pushing the downloader straight into its retry path; past the bound it still
    // receives a retryable proxy response and no live tunnel is released early.
    let lease = match shared.governor.acquire_host_for_tunnel(
        parsed.host.clone(),
        TUNNEL_WAIT,
        shared.headroom,
    ) {
        Some(lease) => lease,
        None => {
            crate::logging::record(
                crate::logging::Event::warn("proxy.reject")
                    .host(parsed.host.clone())
                    .detail(format!(
                        "Yerel ağ aracısı tünel kapasitesi dolu; istek bekleme sonrası reddedildi ({} open={})",
                        shared.governor.usage_summary(&parsed.host),
                        shared.open_tunnels()
                    )),
            );
            let _ = write_proxy_error(
                client,
                503,
                crate::i18n::ui(
                    "Ağ bağlantısı kapasitesi dolu",
                    "Network connection capacity is full",
                ),
            );
            return;
        }
    };
    let mut upstream = match connect_target(&parsed.host, parsed.port, &shared) {
        Ok(stream) => stream,
        Err(_) => {
            let _ = write_proxy_error(
                client,
                502,
                crate::i18n::ui(
                    "Uzak ağ bağlantısı açılamadı",
                    "The remote network connection could not be opened",
                ),
            );
            return;
        }
    };

    let registration = match TunnelRegistration::new(&shared, &client, &upstream) {
        Ok(registration) => registration,
        Err(_) => {
            let _ = write_proxy_error(
                client,
                503,
                crate::i18n::ui(
                    "Yerel ağ aracısı tünel açamadı",
                    "The local network proxy could not open the tunnel",
                ),
            );
            return;
        }
    };
    if parsed.connect {
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .is_err()
        {
            return;
        }
    } else if upstream.write_all(&parsed.forward_head).is_err() {
        let _ = write_proxy_error(
            client,
            502,
            crate::i18n::ui(
                "Uzak ağ isteği gönderilemedi",
                "The remote network request could not be sent",
            ),
        );
        return;
    }
    if !parsed.trailing.is_empty() && upstream.write_all(&parsed.trailing).is_err() {
        return;
    }
    relay(client, upstream, lease, &shared, &parsed.host);
    drop(registration);
}

struct ProxyRequest {
    host: String,
    port: u16,
    connect: bool,
    forward_head: Vec<u8>,
    trailing: Vec<u8>,
}

struct ProxyFrame {
    head: Vec<u8>,
    trailing: Vec<u8>,
}

fn read_proxy_head(client: &mut TcpStream) -> Result<ProxyFrame> {
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    loop {
        let read = client.read(&mut chunk).context(crate::i18n::ui(
            "Yerel ağ aracısı isteği okunamadı",
            "The local network proxy request could not be read",
        ))?;
        if read == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yerel ağ aracısı isteği erken kapandı",
                    "The local network proxy request closed early"
                )
            );
        }
        if buffer.len().saturating_add(read) > PROXY_HEAD_LIMIT {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yerel ağ aracısı başlık sınırı aşıldı",
                    "The local network proxy header limit was exceeded"
                )
            );
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let trailing = buffer.split_off(end + 4);
            return Ok(ProxyFrame {
                head: buffer,
                trailing,
            });
        }
    }
}

fn parse_proxy_request(shared: &ProxyShared, frame: &ProxyFrame) -> Result<ProxyRequest> {
    let head = std::str::from_utf8(&frame.head).context(crate::i18n::ui(
        "Yerel ağ aracısı başlığı UTF-8 değil",
        "The local network proxy header is not UTF-8",
    ))?;
    let mut lines = head.strip_suffix("\r\n\r\n").unwrap_or(head).split("\r\n");
    let request_line = lines.next().context(crate::i18n::ui(
        "Yerel ağ aracısı istek satırı eksik",
        "The local network proxy request line is missing",
    ))?;
    let mut fields = request_line.split_ascii_whitespace();
    let method = fields.next().context(crate::i18n::ui(
        "Yerel ağ aracısı yöntemi eksik",
        "The local network proxy method is missing",
    ))?;
    let target = fields.next().context(crate::i18n::ui(
        "Yerel ağ aracısı hedefi eksik",
        "The local network proxy target is missing",
    ))?;
    let version = fields.next().context(crate::i18n::ui(
        "Yerel ağ aracısı HTTP sürümü eksik",
        "The local network proxy HTTP version is missing",
    ))?;
    if fields.next().is_some()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yerel ağ aracısı istek satırı geçersiz",
                "The local network proxy request line is invalid"
            )
        );
    }

    let mut proxy_authorization = None;
    let mut connection_options = Vec::new();
    let mut retained = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').context(crate::i18n::ui(
            "Yerel ağ aracısı başlığı geçersiz",
            "The local network proxy header is invalid",
        ))?;
        if name.is_empty()
            || !is_http_token(name)
            || value
                .bytes()
                .any(|byte| byte == 0 || byte == 0x7f || (byte < 0x20 && byte != b'\t'))
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yerel ağ aracısı başlığı geçersiz",
                    "The local network proxy header is invalid"
                )
            );
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("proxy-authorization") {
            if proxy_authorization.replace(value.to_string()).is_some() {
                return Err(ProxyAuthRejected(crate::i18n::ui(
                    "Yerel ağ aracısı kimlik başlığı yinelenmiş",
                    "The local network proxy authentication header is duplicated",
                ))
                .into());
            }
            continue;
        }
        if name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("proxy-connection")
        {
            for option in value.split(',').map(str::trim) {
                if !is_http_token(option) {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Yerel ağ aracısı bağlantı başlığı geçersiz",
                            "The local network proxy connection header is invalid"
                        )
                    );
                }
                connection_options.push(option.to_ascii_lowercase());
            }
            continue;
        }
        retained.push((name, value));
    }
    if !proxy_authorization
        .as_deref()
        .is_some_and(|value| capability_matches(shared, value))
    {
        return Err(ProxyAuthRejected(crate::i18n::ui(
            "Yerel ağ aracısı kimliği doğrulanamadı",
            "The local network proxy authentication failed",
        ))
        .into());
    }

    if method == "CONNECT" {
        let (host, port) = proxy_authority(target)?;
        return Ok(ProxyRequest {
            host,
            port,
            connect: true,
            forward_head: Vec::new(),
            trailing: frame.trailing.clone(),
        });
    }

    let target_url = Url::parse(target).context(crate::i18n::ui(
        "Yerel ağ aracısı HTTP hedefi geçersiz",
        "The local network proxy HTTP target is invalid",
    ))?;
    if target_url.scheme() != "http"
        || target_url.host_str().is_none()
        || !target_url.username().is_empty()
        || target_url.password().is_some()
        || target_url.fragment().is_some()
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yerel ağ aracısı yalnız HTTP veya CONNECT hedefi kabul eder",
                "The local network proxy accepts only HTTP or CONNECT targets"
            )
        );
    }
    let host = canonical_host(&target_url)?;
    let port = target_url.port_or_known_default().context(crate::i18n::ui(
        "Yerel ağ aracısı HTTP bağlantı noktası geçersiz",
        "The local network proxy HTTP port is invalid",
    ))?;
    let mut path = target_url.path().to_string();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(query) = target_url.query() {
        path.push('?');
        path.push_str(query);
    }
    let authority = match target_url.host().context(crate::i18n::ui(
        "Yerel ağ aracısı HTTP sunucusu eksik",
        "The local network proxy HTTP host is missing",
    ))? {
        Host::Domain(value) => value.to_owned(),
        Host::Ipv4(value) => value.to_string(),
        Host::Ipv6(value) => format!("[{value}]"),
    };
    let authority = if target_url.port().is_some() {
        format!("{authority}:{port}")
    } else {
        authority
    };
    let mut forward_head =
        format!("{method} {path} {version}\r\nHost: {authority}\r\n").into_bytes();
    for (name, value) in retained {
        if strip_proxy_header(name, &connection_options) {
            continue;
        }
        forward_head.extend_from_slice(name.as_bytes());
        forward_head.extend_from_slice(b": ");
        forward_head.extend_from_slice(value.as_bytes());
        forward_head.extend_from_slice(b"\r\n");
    }
    forward_head.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(ProxyRequest {
        host,
        port,
        connect: false,
        forward_head,
        trailing: frame.trailing.clone(),
    })
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn strip_proxy_header(name: &str, connection_options: &[String]) -> bool {
    // A proxy terminates its own connection semantics. In particular, Chrome
    // impersonation sends h2c Upgrade/HTTP2-Settings through curl_cffi; sending
    // those after replacing Connection with `close` makes an HTTP/1 origin
    // legitimately tear down the request without a response.
    name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("proxy-connection")
        || name.eq_ignore_ascii_case("proxy-authenticate")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("upgrade")
        || name.eq_ignore_ascii_case("http2-settings")
        || connection_options
            .iter()
            .any(|option| name.eq_ignore_ascii_case(option))
}

fn proxy_authority(value: &str) -> Result<(String, u16)> {
    if value.len() > 512
        || value.contains(['/', '?', '#', '@', '\\'])
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yerel ağ aracısı CONNECT hedefi geçersiz",
                "The local network proxy CONNECT target is invalid"
            )
        );
    }

    // `Url::port()` deliberately erases a scheme's explicit default (for
    // example CONNECT `example.test:80`), but CONNECT requires an explicit
    // authority port. Split it before asking `Url` to validate the host.
    let (host_text, port_text, bracketed) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']').context(crate::i18n::ui(
            "Yerel ağ aracısı CONNECT IPv6 hedefi geçersiz",
            "The local network proxy CONNECT IPv6 target is invalid",
        ))?;
        let host = &rest[..end];
        let port = rest[end + 1..].strip_prefix(':').context(crate::i18n::ui(
            "Yerel ağ aracısı CONNECT bağlantı noktası eksik",
            "The local network proxy CONNECT port is missing",
        ))?;
        if port.contains(':') {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yerel ağ aracısı CONNECT bağlantı noktası geçersiz",
                    "The local network proxy CONNECT port is invalid"
                )
            );
        }
        (host, port, true)
    } else {
        let (host, port) = value.rsplit_once(':').context(crate::i18n::ui(
            "Yerel ağ aracısı CONNECT bağlantı noktası eksik",
            "The local network proxy CONNECT port is missing",
        ))?;
        if host.is_empty() || host.contains(':') {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yerel ağ aracısı CONNECT hedefi geçersiz",
                    "The local network proxy CONNECT target is invalid"
                )
            );
        }
        (host, port, false)
    };
    let port: u16 = port_text.parse().context(crate::i18n::ui(
        "Yerel ağ aracısı CONNECT bağlantı noktası geçersiz",
        "The local network proxy CONNECT port is invalid",
    ))?;
    if port == 0 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yerel ağ aracısı CONNECT bağlantı noktası geçersiz",
                "The local network proxy CONNECT port is invalid"
            )
        );
    }
    let authority = if bracketed {
        format!("[{host_text}]")
    } else {
        host_text.to_owned()
    };
    let target = Url::parse(&format!("http://{authority}/")).context(crate::i18n::ui(
        "Yerel ağ aracısı CONNECT hedefi geçersiz",
        "The local network proxy CONNECT target is invalid",
    ))?;
    if target.username() != "" || target.password().is_some() || target.path() != "/" {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yerel ağ aracısı CONNECT hedefi geçersiz",
                "The local network proxy CONNECT target is invalid"
            )
        );
    }
    Ok((canonical_host(&target)?, port))
}

fn connect_target(host: &str, port: u16, shared: &ProxyShared) -> Result<TcpStream> {
    shared
        .last_outbound_at
        .store(now_epoch_ms(), Ordering::Release);
    if shared.stopping.load(Ordering::Acquire) || shared.control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Uzak ağ bağlantısı durduruldu",
                "The remote network connection was stopped"
            )
        );
    }
    // The configured outbound proxy (direct, HTTP CONNECT or SOCKS5) opens the tunnel.
    crate::proxy::current().connect(host, port, PROXY_CONNECT_TIMEOUT)
}

struct TunnelRegistration {
    shared: Arc<ProxyShared>,
    client: u64,
    upstream: u64,
}

impl TunnelRegistration {
    fn new(shared: &Arc<ProxyShared>, client: &TcpStream, upstream: &TcpStream) -> Result<Self> {
        // Complete both fallible clones before mutating the shutdown registry;
        // otherwise a failed upstream clone would strand the client entry.
        let client_clone = client.try_clone()?;
        let upstream_clone = upstream.try_clone()?;
        let client_id = shared.next_tunnel.fetch_add(1, Ordering::Relaxed);
        let upstream_id = shared.next_tunnel.fetch_add(1, Ordering::Relaxed);
        let mut tunnels = shared
            .tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tunnels.insert(client_id, client_clone);
        tunnels.insert(upstream_id, upstream_clone);
        Ok(Self {
            shared: Arc::clone(shared),
            client: client_id,
            upstream: upstream_id,
        })
    }
}

impl Drop for TunnelRegistration {
    fn drop(&mut self) {
        let mut tunnels = self
            .shared
            .tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tunnels.remove(&self.client);
        tunnels.remove(&self.upstream);
    }
}

impl ProxyShared {
    /// Tunnels this child keeps open, idle ones included, for a refused admission's log line.
    fn open_tunnels(&self) -> usize {
        self.tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
            / 2
    }

    fn close_tunnels(&self) {
        let tunnels = self
            .tunnels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for stream in tunnels.values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn relay(
    mut client: TcpStream,
    mut upstream: TcpStream,
    lease: ConnectionLease,
    shared: &ProxyShared,
    host: &str,
) {
    // The child owns application-level retries, but each relay is also bounded
    // so a silent tunnel cannot consume a worker forever. Readiness is polled in
    // short steps because a blocked Winsock recv is not woken by a shutdown
    // issued from another thread; cancellation is checked between steps.
    let _ = upstream.set_write_timeout(Some(PROXY_RELAY_IDLE_TIMEOUT));
    // The server's bytes wait in `answer` until the client takes them: a client that leaves an
    // answer unread (a demuxer that opened every rendition of a stream) moves no bytes, and its
    // tunnel gives the slot back like one that went quiet after the answer. Without a slot the
    // tunnel still hands over what it holds, and takes a slot again before it passes on more
    // bytes from either side; a closed end needs none.
    if client.set_nonblocking(true).is_err() {
        return;
    }
    let mut answer = vec![0u8; RELAY_BUFFER];
    let mut pending = 0..0;
    let mut request = vec![0u8; RELAY_BUFFER];
    let mut lease = Some(lease);
    let mut client_open = true;
    let mut upstream_spoke_last = false;
    let mut last_traffic = Instant::now();
    while !shared.stopping.load(Ordering::Acquire)
        && last_traffic.elapsed() < PROXY_RELAY_IDLE_TIMEOUT
    {
        if upstream_spoke_last && last_traffic.elapsed() >= IDLE_SLOT_RELEASE_AFTER {
            lease = None;
        }
        let client_events = (if client_open { POLLRDNORM } else { 0 })
            | (if pending.is_empty() { 0 } else { POLLOUT });
        let mut polls = [
            (&upstream, if pending.is_empty() { POLLRDNORM } else { 0 }),
            (&client, client_events),
        ]
        .map(|(stream, events)| WSAPOLLFD {
            fd: stream.as_raw_socket() as _,
            events: events as _,
            revents: 0,
        });
        let watched = if client_events == 0 { 1 } else { 2 };
        let ready = unsafe { WSAPoll(polls.as_mut_ptr(), watched, RELAY_POLL.as_millis() as i32) };
        if ready < 0 {
            return;
        }
        if ready == 0 {
            continue;
        }
        let mut moved = false;
        if pending.is_empty() && polls[0].revents != 0 {
            // The server's end closes the tunnel: nothing it sends can follow.
            let read = match upstream.read(&mut answer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            if lease.is_none() {
                lease = reacquire_tunnel_slot(shared, host);
                if lease.is_none() {
                    return;
                }
            }
            pending = 0..read;
            upstream_spoke_last = true;
            last_traffic = Instant::now();
            moved = true;
        }
        if !pending.is_empty() {
            match client.write(&answer[pending.clone()]) {
                Ok(0) => return,
                Ok(written) => {
                    pending.start += written;
                    last_traffic = Instant::now();
                    moved = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        if client_open && polls[1].revents & (POLLRDNORM | POLLHUP | POLLERR) != 0 {
            let forwarded = match client.read(&mut request) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Some(false),
                Ok(0) | Err(_) => None,
                Ok(read) => {
                    if lease.is_none() {
                        lease = reacquire_tunnel_slot(shared, host);
                        if lease.is_none() {
                            return;
                        }
                    }
                    upstream.write_all(&request[..read]).ok().map(|()| true)
                }
            };
            match forwarded {
                Some(false) => {}
                Some(true) => {
                    upstream_spoke_last = false;
                    last_traffic = Instant::now();
                    moved = true;
                }
                None => {
                    // A client half-close still lets the server finish its answer.
                    client_open = false;
                    let _ = upstream.shutdown(Shutdown::Write);
                    upstream_spoke_last = false;
                    last_traffic = Instant::now();
                    moved = true;
                }
            }
        }
        // A closed client is reported ready on every poll while it still has bytes to take.
        if !moved {
            thread::sleep(ACQUIRE_POLL);
        }
    }
}

/// Waits for a slot for an open tunnel that moves bytes again; `None` once the proxy stops.
/// The tunnel holds no slot meanwhile, so the wait cannot block the transfers it waits for.
fn reacquire_tunnel_slot(shared: &ProxyShared, host: &str) -> Option<ConnectionLease> {
    while !shared.stopping.load(Ordering::Acquire) {
        if let Some(lease) =
            shared
                .governor
                .acquire_host_for_tunnel(host.to_owned(), RELAY_POLL, shared.headroom)
        {
            return Some(lease);
        }
    }
    None
}

fn write_proxy_error(mut client: TcpStream, status: u16, message: &str) -> io::Result<()> {
    // Status lines are protocol tokens, not user-facing diagnostics. Keeping
    // this ASCII avoids a client treating a localized reason phrase as an
    // empty/malformed proxy reply and hiding the actionable response body.
    let status_text = match status {
        400 => "Bad Request",
        407 => "Proxy Authentication Required",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Proxy Error",
    };
    let body = message.as_bytes();
    write!(
        client,
        "HTTP/1.1 {status} {status_text}\r\nConnection: close\r\n"
    )?;
    if status == 407 {
        // Some HTTP clients only attach URL credentials after the standard
        // challenge, while others send them preemptively.
        client.write_all(b"Proxy-Authenticate: Basic realm=\"SSDownload\"\r\n")?;
    }
    write!(client, "Content-Length: {}\r\n\r\n", body.len())?;
    client.write_all(body)
}

fn capability_matches(shared: &ProxyShared, value: &str) -> bool {
    let Some((scheme, encoded)) =
        value.split_once(|character: char| character.is_ascii_whitespace())
    else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let mut expected = b"ssdownload:".to_vec();
    expected.extend_from_slice(&shared.capability);
    if decoded.len() != expected.len() {
        return false;
    }
    decoded
        .iter()
        .zip(expected)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::{ExternalProxy, NetworkGovernor, GLOBAL_CONNECTION_BUDGET, REFUSAL_WAITS_FOR};
    use crate::model::TransferControl;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::{
        io::{ErrorKind, Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    /// Opens a CONNECT tunnel through the proxy and returns the stream with the status line.
    fn open_tunnel(proxy: &ExternalProxy, target: &str) -> (TcpStream, String) {
        let (credentials, address) = proxy
            .proxy_url()
            .strip_prefix("http://")
            .and_then(|rest| rest.rsplit_once('@'))
            .expect("proxy url");
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        write!(
            stream,
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {}\r\n\r\n",
            STANDARD.encode(credentials)
        )
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let status = String::from_utf8_lossy(&head)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        (stream, status)
    }

    /// A server that answers every read with "pong" and keeps the connection open.
    fn answering_server() -> String {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("127.0.0.1:{}", server.local_addr().unwrap().port());
        thread::spawn(move || {
            for stream in server.incoming() {
                let Ok(mut stream) = stream else { return };
                thread::spawn(move || {
                    let mut buffer = [0u8; 64];
                    while let Ok(read) = stream.read(&mut buffer) {
                        if read == 0 || stream.write_all(b"pong").is_err() {
                            return;
                        }
                    }
                });
            }
        });
        target
    }

    fn exchange(stream: &mut TcpStream) {
        stream.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"pong");
    }

    fn eventually(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn an_answered_idle_tunnel_gives_its_slot_back_and_stays_open() {
        let target = answering_server();
        let host = || "127.0.0.1".to_owned();
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let (mut pooled, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        exchange(&mut pooled);

        // Silent after its answer, the open tunnel no longer counts against the host.
        assert!(
            eventually(|| governor.try_acquire_host(host()).is_some()),
            "the idle tunnel kept its slot"
        );
        let held = governor.try_acquire_host(host()).expect("free slot");

        // Its next request waits for a slot instead of exceeding the bound.
        pooled
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        pooled.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        let early = pooled.read(&mut reply);
        assert!(
            matches!(&early, Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)),
            "{early:?}"
        );
        drop(held);
        pooled
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        pooled.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"pong");

        // A second tunnel to the same host is admitted while the first stays open.
        assert!(eventually(|| governor.try_acquire_host(host()).is_some()));
        let (mut second, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        exchange(&mut second);
        assert!(eventually(|| governor.try_acquire_host(host()).is_some()));
        exchange(&mut pooled);
    }

    #[test]
    fn a_tunnel_waiting_for_its_answer_keeps_its_slot() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("127.0.0.1:{}", server.local_addr().unwrap().port());
        let (accepted, silent) = std::sync::mpsc::channel();
        thread::spawn(move || {
            if let Ok((stream, _)) = server.accept() {
                let _ = accepted.send(stream);
            }
        });
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let (mut waiting, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        let _upstream = silent.recv_timeout(Duration::from_secs(5)).unwrap();
        waiting.write_all(b"ping").unwrap();
        assert!(!eventually(|| governor
            .try_acquire_host("127.0.0.1".into())
            .is_some()));
    }

    #[test]
    fn a_tunnel_waiting_for_a_slot_closes_when_the_proxy_stops() {
        let target = answering_server();
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let (mut pooled, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        exchange(&mut pooled);
        assert!(eventually(|| governor
            .try_acquire_host("127.0.0.1".into())
            .is_some()));
        let _held = governor.try_acquire_host("127.0.0.1".into()).unwrap();
        pooled.write_all(b"ping").unwrap();

        // Stopping ends the wait: the proxy shuts down and the request is never forwarded.
        let (stopped, done) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(proxy);
            let _ = stopped.send(());
        });
        done.recv_timeout(Duration::from_secs(5))
            .expect("the proxy stopped");
        let mut reply = [0u8; 4];
        let read = pooled.read(&mut reply);
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    }

    #[test]
    fn open_tunnels_counts_each_relayed_tunnel_once() {
        let target = answering_server();
        let proxy =
            ExternalProxy::spawn(NetworkGovernor::new(4), TransferControl::default()).unwrap();
        let (mut first, _) = open_tunnel(&proxy, &target);
        let (mut second, _) = open_tunnel(&proxy, &target);
        exchange(&mut first);
        exchange(&mut second);
        assert_eq!(proxy.shared.open_tunnels(), 2);
        drop(first);
        assert!(eventually(|| proxy.shared.open_tunnels() == 1));
    }

    #[test]
    fn a_tunnel_refused_after_the_wait_gets_a_retryable_503() {
        let target = answering_server();
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let _held = governor.try_acquire_host("127.0.0.1".into()).unwrap();
        let started = Instant::now();
        let (_, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 503"), "{status}");
        assert!(started.elapsed() >= super::TUNNEL_WAIT);
    }

    /// Whether `easy_for_url_before` gets a handle. It is asked from another thread, so that a
    /// wait that never ends fails the test instead of hanging it.
    fn gets_a_handle(
        governor: &NetworkGovernor,
        url: &url::Url,
        control: &TransferControl,
        deadline: Instant,
    ) -> bool {
        let (answer, receiver) = std::sync::mpsc::channel();
        let (governor, url, control) = (governor.clone(), url.clone(), control.clone());
        thread::spawn(move || {
            let _ = answer.send(
                governor
                    .easy_for_url_before(&url, &control, deadline)
                    .is_ok(),
            );
        });
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait for a slot never ended")
    }

    #[test]
    fn an_easy_handle_waits_for_a_free_slot_only_until_its_deadline() {
        let governor = NetworkGovernor::new(1);
        let url = url::Url::parse("http://127.0.0.1:9/file").unwrap();
        let control = TransferControl::default();
        let held = governor.try_acquire_host("127.0.0.1".into()).unwrap();

        let started = Instant::now();
        let deadline = started + Duration::from_millis(300);
        assert!(!gets_a_handle(&governor, &url, &control, deadline));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_secs(3), "{waited:?}");

        // A slot that comes free before the deadline is taken.
        let releasing = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        assert!(gets_a_handle(&governor, &url, &control, deadline));
        releasing.join().unwrap();
    }

    #[test]
    fn a_stop_ends_the_wait_for_a_slot_before_its_deadline() {
        let governor = NetworkGovernor::new(1);
        let url = url::Url::parse("http://127.0.0.1:9/file").unwrap();
        let control = TransferControl::default();
        let _held = governor.try_acquire_host("127.0.0.1".into()).unwrap();
        let stopping = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            stopping.pause();
        });
        let started = Instant::now();
        let deadline = started + Duration::from_secs(8);
        assert!(!gets_a_handle(&governor, &url, &control, deadline));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_tunnel_whose_client_leaves_the_answer_unread_gives_its_slot_back() {
        const ANSWER: usize = 32 * 1024 * 1024;
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("127.0.0.1:{}", server.local_addr().unwrap().port());
        thread::spawn(move || {
            if let Ok((mut stream, _)) = server.accept() {
                let mut request = [0u8; 4];
                if stream.read_exact(&mut request).is_ok() {
                    let _ = stream.write_all(&vec![7u8; ANSWER]);
                }
                thread::sleep(Duration::from_secs(30));
            }
        });
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let (mut unread, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        unread.write_all(b"ping").unwrap();

        // The answer fills the socket buffers while the client reads none of it, as a demuxer
        // does with a rendition it opened and does not need yet.
        assert!(
            eventually(|| governor.try_acquire_host("127.0.0.1".into()).is_some()),
            "the tunnel kept its slot while its client was not reading"
        );

        // With its slot taken meanwhile, the tunnel hands over what it holds and waits for a
        // slot before it passes on more of the answer.
        let held = governor
            .try_acquire_host("127.0.0.1".into())
            .expect("free slot");
        unread
            .set_read_timeout(Some(Duration::from_millis(700)))
            .unwrap();
        let mut received = 0;
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            match unread.read(&mut buffer) {
                Ok(read) => {
                    assert!(read > 0, "the tunnel closed after {received} bytes");
                    assert!(buffer[..read].iter().all(|&byte| byte == 7));
                    received += read;
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break
                }
                Err(error) => panic!("{error}"),
            }
        }
        assert!(received < ANSWER, "the whole answer passed without a slot");

        // The open tunnel still delivers the whole answer once a slot is free.
        drop(held);
        unread
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        while received < ANSWER {
            let read = unread.read(&mut buffer).unwrap();
            assert!(read > 0, "the tunnel closed after {received} bytes");
            assert!(buffer[..read].iter().all(|&byte| byte == 7));
            received += read;
        }
        assert_eq!(received, ANSWER);
    }

    #[test]
    fn a_tunnel_without_a_slot_closes_with_its_client_without_waiting_for_one() {
        let target = answering_server();
        let governor = NetworkGovernor::new(1);
        let proxy = ExternalProxy::spawn(governor.clone(), TransferControl::default()).unwrap();
        let (mut pooled, status) = open_tunnel(&proxy, &target);
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        exchange(&mut pooled);
        assert!(eventually(|| governor
            .try_acquire_host("127.0.0.1".into())
            .is_some()));
        let _held = governor.try_acquire_host("127.0.0.1".into()).unwrap();
        drop(pooled);
        assert!(
            eventually(|| proxy.shared.open_tunnels() == 0),
            "the closed tunnel waited for a slot"
        );
    }

    fn proxy_reply(proxy: &ExternalProxy, request: &str) -> String {
        let address = proxy
            .proxy_url()
            .rsplit('@')
            .next()
            .expect("proxy address")
            .to_owned();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn the_loopback_proxy_challenges_a_missing_or_wrong_credential_with_407() {
        let proxy =
            ExternalProxy::spawn(NetworkGovernor::new(2), TransferControl::default()).unwrap();
        let missing = proxy_reply(
            &proxy,
            "CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\n\r\n",
        );
        assert!(missing.starts_with("HTTP/1.1 407 "), "{missing}");
        assert!(missing.contains("Proxy-Authenticate: Basic"), "{missing}");
        let wrong = proxy_reply(
            &proxy,
            "CONNECT example.test:443 HTTP/1.1\r\nHost: example.test:443\r\nProxy-Authorization: Basic c3Nkb3dubG9hZDp3cm9uZw==\r\n\r\n",
        );
        assert!(wrong.starts_with("HTTP/1.1 407 "), "{wrong}");
        let malformed = proxy_reply(&proxy, "NOT A REQUEST\r\n\r\n");
        assert!(malformed.starts_with("HTTP/1.1 400 "), "{malformed}");
    }

    #[test]
    fn a_full_host_waits_for_a_slot_before_the_proxy_refuses() {
        let limited = NetworkGovernor::new(1);
        let held = limited
            .try_acquire_host("example.test".into())
            .expect("first slot");
        let started = Instant::now();
        assert!(limited
            .acquire_host_for_tunnel("example.test".into(), Duration::from_millis(200), 0)
            .is_none());
        assert!(started.elapsed() >= Duration::from_millis(150));
        drop(held);
        assert!(limited
            .acquire_host_for_tunnel("example.test".into(), Duration::from_millis(500), 0)
            .is_some());
    }

    #[test]
    fn a_requester_counts_as_waiting_for_its_host_until_it_has_a_slot_or_gives_up() {
        let governor = NetworkGovernor::new(1);
        let held = governor
            .try_acquire_host("example.test".into())
            .expect("first slot");
        let control = TransferControl::default();
        thread::scope(|scope| {
            let tunnel = scope.spawn(|| {
                governor.acquire_host_for_tunnel("example.test".into(), Duration::from_secs(5), 0)
            });
            assert!(eventually(|| governor.host_has_waiters("example.test")));
            assert!(!governor.host_has_waiters("other.test"));
            drop(held);
            let lease = tunnel.join().unwrap().expect("the freed slot");
            assert!(!governor.host_has_waiters("example.test"));

            // A wildcard waits on every host, a requester that gives up stops waiting.
            let wildcard = scope.spawn(|| governor.acquire_wildcard(&control));
            assert!(eventually(|| governor.host_has_waiters("other.test")));
            control.cancel();
            assert!(wildcard.join().unwrap().is_err());
            assert!(!governor.host_has_waiters("other.test"));
            drop(lease);
        });
    }

    #[test]
    fn a_request_refused_without_waiting_counts_as_waiting_for_a_while() {
        let governor = NetworkGovernor::new(1);
        let _held = governor
            .try_acquire_host("example.test".into())
            .expect("first slot");
        assert!(!governor.host_has_waiters("example.test"));

        // A refused host slot counts for its host; a refused wildcard slot, a browser
        // transfer's permit, for every host.
        assert!(governor.try_acquire_host("example.test".into()).is_none());
        assert!(governor.host_has_waiters("example.test"));
        assert!(!governor.host_has_waiters("other.test"));
        assert!(governor.try_acquire_wildcard().is_none());
        assert!(governor.host_has_waiters("other.test"));

        let long_ago = Instant::now().checked_sub(REFUSAL_WAITS_FOR).unwrap();
        {
            let mut state = governor.lock_state();
            state.refused.values_mut().for_each(|at| *at = long_ago);
            state.refused_wildcard = Some(long_ago);
        }
        assert!(!governor.host_has_waiters("example.test"));
        assert!(!governor.host_has_waiters("other.test"));
    }

    #[test]
    fn a_requester_held_back_by_the_global_budget_counts_for_every_host() {
        let governor = NetworkGovernor::new(16);
        let held = ["a.test", "b.test"]
            .into_iter()
            .flat_map(|host| (0..16).map(move |_| host))
            .map(|host| governor.try_acquire_host(host.into()).expect("a slot"))
            .collect::<Vec<_>>();
        assert_eq!(held.len(), GLOBAL_CONNECTION_BUDGET);
        assert!(!governor.host_has_waiters("a.test"));
        thread::scope(|scope| {
            let tunnel = scope.spawn(|| {
                governor.acquire_host_for_tunnel("c.test".into(), Duration::from_secs(5), 0)
            });
            assert!(eventually(|| governor.host_has_waiters("a.test")));
            drop(held);
            assert!(tunnel.join().unwrap().is_some());
        });
        assert!(!governor.host_has_waiters("a.test"));
    }

    #[test]
    fn an_ftp_reservation_counts_as_waiting_for_its_host_until_it_has_both_slots() {
        let governor = NetworkGovernor::new(2);
        let held = governor
            .try_acquire_host("ftp.test".into())
            .expect("one slot");
        let control = TransferControl::default();
        thread::scope(|scope| {
            let reservation = scope.spawn(|| governor.reserve_ftp("ftp.test".into(), &control));
            assert!(eventually(|| governor.host_has_waiters("ftp.test")));
            drop(held);
            assert!(reservation.join().unwrap().is_ok());
        });
        assert!(!governor.host_has_waiters("ftp.test"));
    }

    #[test]
    fn per_host_limit_accessor_reports_the_active_bound() {
        let governor = NetworkGovernor::new(2);
        assert_eq!(governor.per_host_limit(), 2);
        governor.set_per_host_limit(9);
        assert_eq!(governor.per_host_limit(), 9);
        // A zero bound is normalized to one like every admission path.
        governor.set_per_host_limit(0);
        assert_eq!(governor.per_host_limit(), 1);
    }

    #[test]
    fn wildcard_admission_counts_empty_and_named_hosts() {
        let governor = NetworkGovernor::new(2);
        let first = governor
            .try_acquire_wildcard()
            .expect("first wildcard request has capacity");
        let second = governor
            .try_acquire_wildcard()
            .expect("second wildcard request has capacity");
        assert!(governor.try_acquire_wildcard().is_none());

        drop(second);
        let replacement = governor
            .try_acquire_wildcard()
            .expect("releasing wildcard capacity admits another request");
        drop(replacement);

        let named = governor
            .try_acquire_host("media.example".into())
            .expect("named host shares remaining wildcard capacity");
        assert!(governor.try_acquire_host("media.example".into()).is_none());

        drop(first);
        assert!(governor.try_acquire_host("media.example".into()).is_some());
        drop(named);
    }

    #[test]
    fn host_names_typed_in_the_settings_share_the_canonical_form_of_a_url_host() {
        let from_url = |url: &str| super::canonical_host(&url::Url::parse(url).unwrap()).unwrap();
        for typed in [
            "bücher.example",
            "BÜCHER.example",
            "xn--bcher-kva.example.",
            " Xn--Bcher-Kva.Example ",
        ] {
            assert_eq!(
                super::canonical_host_name(typed).as_deref(),
                Some("xn--bcher-kva.example"),
                "{typed}"
            );
        }
        assert_eq!(
            from_url("https://BÜCHER.example./a"),
            "xn--bcher-kva.example"
        );
        assert_eq!(
            super::canonical_host_name("[0:0:0:0:0:0:0:1]").as_deref(),
            Some("::1")
        );
        assert_eq!(
            super::canonical_host_name("10.0.0.1").unwrap(),
            from_url("http://10.0.0.1/")
        );
        for invalid in ["", ".", "a b", "a/b"] {
            assert_eq!(super::canonical_host_name(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn less_than_a_millisecond_left_is_no_time_left() {
        let now = Instant::now();
        let left = |left: Duration| super::time_left(now + left, now);
        assert_eq!(left(Duration::ZERO), None);
        assert_eq!(left(Duration::from_nanos(1)), None);
        assert_eq!(left(Duration::from_micros(999)), None);
        assert_eq!(
            left(Duration::from_millis(1)),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            left(Duration::from_micros(1500)),
            Some(Duration::from_micros(1500))
        );
        assert_eq!(left(Duration::from_secs(30)), Some(Duration::from_secs(30)));
        assert_eq!(
            super::time_left(now, now + Duration::from_secs(1)),
            None,
            "a deadline in the past"
        );
    }
}
