//! A configurable HTTP/1.1 loopback server for tests. It speaks keep-alive, serves a
//! deterministic patterned body for any path, and records every request.

use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

/// How long a stalled response waits for a request to take part of its range.
const STALL_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSupport {
    /// Single ranges answer 206; an unsatisfiable range answers 416.
    Honour,
    /// `Range` is ignored and the full body answers 200.
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EtagKind {
    Strong,
    Weak,
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryAfter {
    Seconds(u64),
    HttpDate(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverLimit {
    /// Answers 503 and closes the connection.
    Unavailable(Option<RetryAfter>),
    /// Closes the socket without reading the request, which resets the connection when the
    /// request has already arrived.
    Reset,
}

/// Cuts the connection after `after_bytes` body bytes on each of the first `responses`
/// responses that carry a longer body.
#[derive(Clone, Copy, Debug)]
pub struct CutBody {
    pub after_bytes: u64,
    pub responses: usize,
}

/// One scripted answer with an empty body. The script is consumed in order, one entry per
/// request of any method, before normal service resumes.
#[derive(Clone, Debug)]
pub struct Scripted {
    pub status: u16,
    pub retry_after: Option<RetryAfter>,
}

/// `from` redirects to `target`, which serves normally for its first
/// `target_requests_before_403` requests and answers 403 afterwards. Once the target has
/// refused a request, `from` redirects to `fresh_target`, which serves normally.
#[derive(Clone, Debug)]
pub struct Redirect {
    pub from: String,
    pub target: String,
    pub fresh_target: String,
    pub target_requests_before_403: usize,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub size: u64,
    pub ranges: RangeSupport,
    pub accept_ranges_header: bool,
    /// `Content-Length` of the `HEAD` answer when it should differ from the body size.
    pub head_content_length: Option<u64>,
    pub head_not_allowed: bool,
    /// The `HEAD` answer carries this `ETag` instead of the one a `GET` gets.
    pub head_etag: Option<String>,
    /// The `HEAD` answer carries no validators and no `Content-Length`.
    pub head_bare: bool,
    /// Without it a `GET` answer has no `Content-Length` and its body ends with the connection.
    pub get_length_header: bool,
    pub etag: EtagKind,
    /// Every request gets its own ETag for the same body.
    pub vary_etag: bool,
    pub last_modified: bool,
    /// Requests from this index on (counting every request, from 0) get the second body
    /// version and its validators.
    pub switch_version_after: Option<usize>,
    /// A `Range` request is answered 206 whatever its `If-Range` says.
    pub ignore_if_range: bool,
    /// A 206 answer to a request with `If-Range` carries no `ETag` and no `Last-Modified`.
    pub partial_without_validators: bool,
    /// A 206 answer to a request with `If-Range` carries no `ETag` but keeps `Last-Modified`.
    pub partial_without_etag: bool,
    /// Both body versions carry the first version's `Last-Modified`.
    pub last_modified_fixed: bool,
    /// Bytes per second on every connection.
    pub throttle: Option<u64>,
    /// Bytes per second on the k-th admitted connection (from 0, counted in the order the log
    /// records their first requests) whose first request is a GET with a `Range` starting
    /// above 0, replacing `throttle` there. Probes, refused connections and connections that
    /// start otherwise do not count.
    pub slow_connection: Option<(usize, u64)>,
    /// The body of the k-th connection, counted as for `slow_connection`, stops after this many
    /// bytes. It goes on once a request for a range that starts inside the body arrives, the
    /// client closes the connection, or `STALL_LIMIT` has passed.
    pub stall: Option<(usize, u64)>,
    /// A stalled body, once a request takes part of its range, goes on only up to the start of
    /// that part and then stays silent until the client closes the connection or `STALL_LIMIT`
    /// has passed.
    pub hold_at_takeover: bool,
    pub max_connections: Option<usize>,
    pub over_limit: OverLimit,
    pub cut_body: Option<CutBody>,
    /// Each cut response lets this many more body bytes through than the one before it.
    pub cut_growth: u64,
    /// Only responses whose body starts at or after this offset are cut.
    pub cut_from: u64,
    /// The ETag changes after every this many `HEAD` requests, so that the requests of one
    /// attempt (a `HEAD` and the transfers after it) share a tag.
    pub etag_rotation_heads: Option<usize>,
    pub scripted: Vec<Scripted>,
    pub redirect: Option<Redirect>,
    /// Request headers copied into the log, matched case-insensitively.
    pub logged_headers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            size: 0,
            ranges: RangeSupport::Honour,
            accept_ranges_header: true,
            head_content_length: None,
            head_not_allowed: false,
            head_etag: None,
            head_bare: false,
            get_length_header: true,
            etag: EtagKind::Strong,
            vary_etag: false,
            last_modified: true,
            switch_version_after: None,
            ignore_if_range: false,
            partial_without_validators: false,
            partial_without_etag: false,
            last_modified_fixed: false,
            throttle: None,
            slow_connection: None,
            stall: None,
            hold_at_takeover: false,
            max_connections: None,
            over_limit: OverLimit::Unavailable(None),
            cut_body: None,
            cut_growth: 0,
            cut_from: 0,
            etag_rotation_heads: None,
            scripted: Vec::new(),
            redirect: None,
            logged_headers: vec!["authorization".into(), "cookie".into(), "referer".into()],
        }
    }
}

/// One received request and the status it was answered with.
#[derive(Clone, Debug)]
pub struct Logged {
    /// Index of the TCP connection, from 0 in accept order.
    pub connection: usize,
    pub method: String,
    pub path: String,
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub headers: Vec<(String, String)>,
    pub status: u16,
    pub body_cut: bool,
}

impl Logged {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Shared {
    config: Config,
    requests: AtomicUsize,
    cuts: AtomicUsize,
    heads: AtomicUsize,
    scripted: AtomicUsize,
    target_requests: AtomicUsize,
    ranged_connections: AtomicUsize,
    /// Silent bodies that ended because the client closed the connection.
    abandoned: AtomicUsize,
    /// Index of the first request that gets the second body version.
    switch_from: AtomicUsize,
    open: AtomicUsize,
    body_bytes: AtomicU64,
    stop: AtomicBool,
    log: Mutex<Vec<Logged>>,
    sockets: Mutex<Vec<TcpStream>>,
}

pub struct Server {
    address: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let switch_from = AtomicUsize::new(config.switch_version_after.unwrap_or(usize::MAX));
        let shared = Arc::new(Shared {
            config,
            switch_from,
            requests: AtomicUsize::new(0),
            cuts: AtomicUsize::new(0),
            heads: AtomicUsize::new(0),
            scripted: AtomicUsize::new(0),
            target_requests: AtomicUsize::new(0),
            ranged_connections: AtomicUsize::new(0),
            abandoned: AtomicUsize::new(0),
            open: AtomicUsize::new(0),
            body_bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            log: Mutex::new(Vec::new()),
            sockets: Mutex::new(Vec::new()),
        });
        let accept = {
            let shared = shared.clone();
            thread::spawn(move || accept_loop(listener, shared))
        };
        Self {
            address,
            shared,
            accept: Some(accept),
        }
    }

    /// The address of `path`, which starts with `/`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn log(&self) -> Vec<Logged> {
        self.shared.log.lock().unwrap().clone()
    }

    /// Every request from now on gets the second body version and its validators.
    pub fn switch_version_now(&self) {
        let seen = self.shared.requests.load(Ordering::Acquire);
        self.shared.switch_from.store(seen, Ordering::Release);
    }

    /// Body bytes written to sockets so far, whether or not the client read them.
    pub fn body_bytes_sent(&self) -> u64 {
        self.shared.body_bytes.load(Ordering::Acquire)
    }

    /// How many bodies that `hold_at_takeover` left silent ended because the client closed the
    /// connection rather than because `STALL_LIMIT` passed.
    pub fn abandoned_holds(&self) -> usize {
        self.shared.abandoned.load(Ordering::Acquire)
    }

    /// Blocks until every connection has closed and no request has arrived for a moment, so
    /// that the log holds every request the client sent before it stopped.
    pub fn wait_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = usize::MAX;
        let mut settled = 0;
        while settled < 3 {
            let requests = self.shared.requests.load(Ordering::Acquire);
            if self.shared.open.load(Ordering::Acquire) == 0 && requests == seen {
                settled += 1;
            } else {
                settled = 0;
            }
            seen = requests;
            assert!(Instant::now() < deadline, "the server never went idle");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        for socket in self.shared.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

/// The byte at `offset` of body `version` (1 or 2). Every offset differs between versions.
fn pattern_byte(version: u8, offset: u64) -> u8 {
    ((offset ^ (offset >> 17))
        .wrapping_add(u64::from(version))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        >> 56) as u8
}

fn fill(version: u8, offset: u64, out: &mut [u8]) {
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = pattern_byte(version, offset + index as u64);
    }
}

pub fn pattern(version: u8, size: u64) -> Vec<u8> {
    let mut body = vec![0; size as usize];
    fill(version, 0, &mut body);
    body
}

pub fn pattern_sha256(version: u8, size: u64) -> String {
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; 1 << 20];
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(chunk.len() as u64) as usize;
        fill(version, offset, &mut chunk[..length]);
        hasher.update(&chunk[..length]);
        offset += length as u64;
    }
    hex::encode(hasher.finalize())
}

pub fn file_sha256(path: &Path) -> String {
    let mut file = File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    hex::encode(hasher.finalize())
}

/// A deterministic generator for tests that vary their input: a seed gives the same numbers on
/// every run.
pub struct Sequence(u64);

impl Sequence {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `bound`, which must not be 0.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        // Tests that start together with the same label must not share a directory.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssdownload-loopback-{label}-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    let mut next_connection = 0;
    while !shared.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let _ = stream.set_nodelay(true);
                let connection = next_connection;
                next_connection += 1;
                let open = shared.open.fetch_add(1, Ordering::AcqRel) + 1;
                let over_limit = shared
                    .config
                    .max_connections
                    .is_some_and(|maximum| open > maximum);
                if over_limit && shared.config.over_limit == OverLimit::Reset {
                    shared.open.fetch_sub(1, Ordering::AcqRel);
                    drop(stream);
                    continue;
                }
                if let Ok(registered) = stream.try_clone() {
                    shared.sockets.lock().unwrap().push(registered);
                }
                let shared = shared.clone();
                thread::spawn(move || {
                    serve(&shared, stream, connection, over_limit);
                    shared.open.fetch_sub(1, Ordering::AcqRel);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Some(Request {
                method,
                path,
                headers,
            });
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
}

fn serve(shared: &Shared, stream: TcpStream, connection: usize, over_limit: bool) {
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut stream = stream;
    let mut place = None;
    while let Some(request) = read_request(&mut reader) {
        if !answer(
            shared,
            &mut stream,
            connection,
            &request,
            over_limit,
            &mut place,
        ) {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
}

/// Where a range request starts.
fn range_start(spec: &str) -> Option<u64> {
    spec.strip_prefix("bytes=")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Judged on an admitted connection's first request: its place, from 0, among the connections
/// that start with a GET whose range begins above 0; `None` for any other connection.
fn ranged_place(shared: &Shared, request: &Request) -> Option<usize> {
    let above_zero = request.method == "GET"
        && request
            .header("range")
            .and_then(range_start)
            .is_some_and(|start| start > 0);
    above_zero.then(|| shared.ranged_connections.fetch_add(1, Ordering::AcqRel))
}

/// Whether the client has closed a non-blocking connection.
fn closed_by_client(stream: &TcpStream) -> bool {
    match stream.peek(&mut [0]) {
        Ok(0) => true,
        Err(error) => error.kind() != std::io::ErrorKind::WouldBlock,
        Ok(_) => false,
    }
}

/// Holds a response, whose body is `start..end` and whose request is logged at `position`, until
/// another request has been logged for a range that starts inside the body, or `STALL_LIMIT` has
/// passed. Returns where the part taken starts, or `end` after the limit; `None` when the client
/// closed the connection meanwhile.
fn wait_for_takeover(
    shared: &Shared,
    stream: &TcpStream,
    position: usize,
    start: u64,
    end: u64,
) -> Option<u64> {
    let deadline = Instant::now() + STALL_LIMIT;
    let _ = stream.set_nonblocking(true);
    let taken = loop {
        let taken = shared
            .log
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != position)
            .filter_map(|(_, entry)| entry.range.as_deref().and_then(range_start))
            .filter(|&from| from > start && from < end)
            .min();
        if taken.is_some() {
            break taken;
        }
        if Instant::now() >= deadline {
            break Some(end);
        }
        if closed_by_client(stream) {
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };
    let _ = stream.set_nonblocking(false);
    taken
}

/// Keeps a response silent until the client closes the connection, which is counted, or
/// `STALL_LIMIT` has passed.
fn hold(shared: &Shared, stream: &TcpStream) {
    let deadline = Instant::now() + STALL_LIMIT;
    let _ = stream.set_nonblocking(true);
    while Instant::now() < deadline {
        if closed_by_client(stream) {
            shared.abandoned.fetch_add(1, Ordering::AcqRel);
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = stream.set_nonblocking(false);
}

struct Reply {
    status: u16,
    headers: Vec<String>,
    length: u64,
    /// Offset and length of the body bytes that follow the head.
    body: Option<(u64, u64)>,
    version: u8,
    close: bool,
    omit_length: bool,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            length: 0,
            body: None,
            version: 1,
            close: false,
            omit_length: false,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

fn retry_after_header(value: &RetryAfter) -> String {
    match value {
        RetryAfter::Seconds(seconds) => format!("Retry-After: {seconds}"),
        RetryAfter::HttpDate(date) => format!("Retry-After: {date}"),
    }
}

enum Wanted {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}

/// A single `bytes=` range; anything else is served as the full body, as a server may.
fn wanted(spec: &str, size: u64) -> Wanted {
    let Some(spec) = spec.trim().strip_prefix("bytes=") else {
        return Wanted::Full;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Wanted::Full;
    };
    match (first.trim(), last.trim()) {
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => Wanted::Unsatisfiable,
            Ok(_) if size == 0 => Wanted::Unsatisfiable,
            Ok(count) => Wanted::Part(size.saturating_sub(count), size - 1),
            Err(_) => Wanted::Full,
        },
        (first, last) => {
            let Ok(start) = first.parse::<u64>() else {
                return Wanted::Full;
            };
            let end = if last.is_empty() {
                u64::MAX
            } else if let Ok(end) = last.parse::<u64>() {
                end
            } else {
                return Wanted::Full;
            };
            if end < start {
                Wanted::Full
            } else if start >= size {
                Wanted::Unsatisfiable
            } else {
                Wanted::Part(start, end.min(size - 1))
            }
        }
    }
}

fn plan(shared: &Shared, request: &Request, index: usize, over_limit: bool) -> Reply {
    let config = &shared.config;
    if over_limit {
        let mut reply = Reply::empty(503);
        if let OverLimit::Unavailable(Some(retry_after)) = &config.over_limit {
            reply.headers.push(retry_after_header(retry_after));
        }
        reply.close = true;
        return reply;
    }
    let step = shared.scripted.fetch_add(1, Ordering::AcqRel);
    if let Some(scripted) = config.scripted.get(step) {
        let mut reply = Reply::empty(scripted.status);
        if let Some(retry_after) = &scripted.retry_after {
            reply.headers.push(retry_after_header(retry_after));
        }
        return reply;
    }
    if let Some(redirect) = &config.redirect {
        if request.path == redirect.from {
            let refused = shared.target_requests.load(Ordering::Acquire)
                > redirect.target_requests_before_403;
            let location = if refused {
                &redirect.fresh_target
            } else {
                &redirect.target
            };
            let mut reply = Reply::empty(302);
            reply.headers.push(format!("Location: {location}"));
            return reply;
        }
        if request.path == redirect.target
            && shared.target_requests.fetch_add(1, Ordering::AcqRel)
                >= redirect.target_requests_before_403
        {
            return Reply::empty(403);
        }
    }

    let version = if index >= shared.switch_from.load(Ordering::Acquire) {
        2
    } else {
        1
    };
    let etag = match config.etag {
        EtagKind::Absent => None,
        kind => {
            let weak = if kind == EtagKind::Weak { "W/" } else { "" };
            let unique = if config.vary_etag {
                format!("-{index}")
            } else if let Some(every) = config.etag_rotation_heads {
                let heads = if request.method == "HEAD" {
                    shared.heads.fetch_add(1, Ordering::AcqRel) + 1
                } else {
                    shared.heads.load(Ordering::Acquire)
                };
                format!("-e{}", heads.saturating_sub(1) / every)
            } else {
                String::new()
            };
            Some(format!("{weak}\"v{version}{unique}\""))
        }
    };
    let last_modified =
        config
            .last_modified
            .then_some(if version == 1 || config.last_modified_fixed {
                "Mon, 01 Jan 2024 00:00:00 GMT"
            } else {
                "Tue, 02 Jan 2024 00:00:00 GMT"
            });
    let mut identity = Vec::new();
    if config.accept_ranges_header {
        identity.push("Accept-Ranges: bytes".to_owned());
    }
    if let Some(etag) = &etag {
        identity.push(format!("ETag: {etag}"));
    }
    if let Some(date) = last_modified {
        identity.push(format!("Last-Modified: {date}"));
    }

    let size = config.size;
    match request.method.as_str() {
        "HEAD" if config.head_not_allowed => {
            let mut reply = Reply::empty(405);
            reply.headers.push("Allow: GET".into());
            reply
        }
        "HEAD" if config.head_bare => Reply {
            omit_length: true,
            ..Reply::empty(200)
        },
        "HEAD" => Reply {
            headers: match &config.head_etag {
                Some(head_etag) => identity
                    .into_iter()
                    .map(|header| {
                        if header.starts_with("ETag:") {
                            format!("ETag: {head_etag}")
                        } else {
                            header
                        }
                    })
                    .collect(),
                None => identity,
            },
            length: config.head_content_length.unwrap_or(size),
            version,
            ..Reply::empty(200)
        },
        "GET" => {
            let if_range_holds = config.ignore_if_range
                || request.header("if-range").is_none_or(|validator| {
                    if validator.starts_with('"') {
                        etag.as_deref() == Some(validator)
                    } else {
                        last_modified == Some(validator)
                    }
                });
            let slice = match request.header("range") {
                Some(spec) if config.ranges == RangeSupport::Honour && if_range_holds => {
                    wanted(spec, size)
                }
                _ => Wanted::Full,
            };
            match slice {
                Wanted::Full => Reply {
                    headers: identity,
                    length: size,
                    body: Some((0, size)),
                    version,
                    close: !config.get_length_header,
                    omit_length: !config.get_length_header,
                    ..Reply::empty(200)
                },
                Wanted::Part(start, end) => {
                    let mut headers = identity;
                    if config.partial_without_validators && request.header("if-range").is_some() {
                        headers.retain(|header| header.starts_with("Accept-Ranges"));
                    }
                    if config.partial_without_etag && request.header("if-range").is_some() {
                        headers.retain(|header| !header.starts_with("ETag:"));
                    }
                    headers.push(format!("Content-Range: bytes {start}-{end}/{size}"));
                    Reply {
                        headers,
                        length: end - start + 1,
                        body: Some((start, end - start + 1)),
                        version,
                        ..Reply::empty(206)
                    }
                }
                Wanted::Unsatisfiable => {
                    let mut reply = Reply::empty(416);
                    reply.headers.push(format!("Content-Range: bytes */{size}"));
                    reply
                }
            }
        }
        _ => Reply::empty(405),
    }
}

/// Answers one request; `false` when the connection must not be used again.
fn answer(
    shared: &Shared,
    stream: &mut TcpStream,
    connection: usize,
    request: &Request,
    over_limit: bool,
    place: &mut Option<Option<usize>>,
) -> bool {
    let config = &shared.config;
    let index = shared.requests.fetch_add(1, Ordering::AcqRel);
    let reply = plan(shared, request, index, over_limit);
    let cut_after = config.cut_body.and_then(|cut| {
        let (start, length) = reply.body?;
        if start < config.cut_from || length <= cut.after_bytes {
            return None;
        }
        let cut_index = shared.cuts.fetch_add(1, Ordering::AcqRel);
        (cut_index < cut.responses)
            .then(|| (cut.after_bytes + cut_index as u64 * config.cut_growth).min(length))
    });
    let mut log = shared.log.lock().unwrap();
    let place = *place.get_or_insert_with(|| {
        (!over_limit)
            .then(|| ranged_place(shared, request))
            .flatten()
    });
    let position = log.len();
    log.push(Logged {
        connection,
        method: request.method.clone(),
        path: request.path.clone(),
        range: request.header("range").map(str::to_owned),
        if_range: request.header("if-range").map(str::to_owned),
        headers: config
            .logged_headers
            .iter()
            .filter_map(|name| {
                let name = name.to_ascii_lowercase();
                request.header(&name).map(|value| (name, value.to_owned()))
            })
            .collect(),
        status: reply.status,
        body_cut: cut_after.is_some(),
    });
    drop(log);

    let mut head = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for header in &reply.headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    if !reply.omit_length {
        head.push_str(&format!("Content-Length: {}\r\n", reply.length));
    }
    if reply.close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return false;
    }
    let Some((start, length)) = reply.body else {
        return !reply.close;
    };
    let rate = match config.slow_connection {
        Some((slow, rate)) if place == Some(slow) => Some(rate),
        _ => config.throttle,
    };
    let sent = cut_after.unwrap_or(length);
    let stall = config
        .stall
        .filter(|&(stalled, after)| place == Some(stalled) && after < sent)
        .map(|(_, after)| after);
    let body = &shared.body_bytes;
    let delivered = match stall {
        Some(after) => {
            write_body(stream, body, reply.version, start, after, rate)
                && match wait_for_takeover(shared, stream, position, start, start + length) {
                    None => false,
                    Some(taken) if config.hold_at_takeover => {
                        let until = taken.saturating_sub(start + after);
                        if write_body(stream, body, reply.version, start + after, until, rate) {
                            hold(shared, stream);
                        }
                        false
                    }
                    Some(_) => write_body(
                        stream,
                        body,
                        reply.version,
                        start + after,
                        sent - after,
                        rate,
                    ),
                }
        }
        None => write_body(stream, body, reply.version, start, sent, rate),
    };
    delivered && sent == length && !reply.close
}

fn write_body(
    stream: &mut TcpStream,
    counter: &AtomicU64,
    version: u8,
    start: u64,
    length: u64,
    rate: Option<u64>,
) -> bool {
    let chunk_size = match rate {
        Some(rate) => (rate / 50).clamp(1024, 64 * 1024) as usize,
        None => 256 * 1024,
    };
    let mut chunk = vec![0; chunk_size];
    let began = Instant::now();
    let mut sent = 0;
    while sent < length {
        let count = (length - sent).min(chunk_size as u64) as usize;
        fill(version, start + sent, &mut chunk[..count]);
        if stream.write_all(&chunk[..count]).is_err() {
            return false;
        }
        counter.fetch_add(count as u64, Ordering::AcqRel);
        sent += count as u64;
        if let Some(rate) = rate {
            let due = Duration::from_secs_f64(sent as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(began.elapsed()) {
                thread::sleep(wait);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Answer {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Answer {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn connect(server: &Server) -> BufReader<TcpStream> {
        let stream = TcpStream::connect(server.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        BufReader::new(stream)
    }

    fn read_head(connection: &mut BufReader<TcpStream>) -> Option<(u16, Vec<(String, String)>)> {
        let mut line = String::new();
        if connection.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let status = line.split_whitespace().nth(1)?.parse().ok()?;
        let mut headers = Vec::new();
        loop {
            line.clear();
            connection.read_line(&mut line).ok()?;
            let line = line.trim_end();
            if line.is_empty() {
                return Some((status, headers));
            }
            let (name, value) = line.split_once(':')?;
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }

    /// Sends one request and reads its answer, or `None` when the connection ends first.
    fn try_request(
        connection: &mut BufReader<TcpStream>,
        method: &str,
        path: &str,
        headers: &[&str],
    ) -> Option<Answer> {
        let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
        for header in headers {
            text.push_str(header);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        connection.get_mut().write_all(text.as_bytes()).ok()?;
        let (status, headers) = read_head(connection)?;
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0; if method == "HEAD" { 0 } else { length }];
        connection.read_exact(&mut body).ok()?;
        Some(Answer {
            status,
            headers,
            body,
        })
    }

    fn request(
        connection: &mut BufReader<TcpStream>,
        method: &str,
        path: &str,
        headers: &[&str],
    ) -> Answer {
        try_request(connection, method, path, headers).expect("the server closed the connection")
    }

    fn config(size: u64) -> Config {
        Config {
            size,
            ..Config::default()
        }
    }

    #[test]
    fn head_and_get_serve_the_patterned_body_over_one_keep_alive_connection() {
        let server = Server::start(Config {
            logged_headers: vec!["Authorization".into(), "Referer".into()],
            ..config(1000)
        });
        let mut connection = connect(&server);
        let head = request(&mut connection, "HEAD", "/a.bin", &[]);
        assert_eq!(head.status, 200);
        assert_eq!(head.header("Content-Length"), Some("1000"));
        assert_eq!(head.header("Accept-Ranges"), Some("bytes"));
        assert_eq!(head.header("ETag"), Some("\"v1\""));
        assert!(head.header("Last-Modified").is_some());
        assert!(head.body.is_empty());
        let get = request(
            &mut connection,
            "GET",
            "/a.bin",
            &["Authorization: Bearer t", "Cookie: hidden=1"],
        );
        assert_eq!(get.status, 200);
        assert_eq!(get.body, pattern(1, 1000));
        let other = request(&mut connect(&server), "GET", "/b.bin", &[]);
        assert_eq!(other.body, pattern(1, 1000));

        let log = server.log();
        assert_eq!(
            log.iter()
                .map(|entry| (entry.connection, entry.method.as_str(), entry.path.as_str()))
                .collect::<Vec<_>>(),
            [
                (0, "HEAD", "/a.bin"),
                (0, "GET", "/a.bin"),
                (1, "GET", "/b.bin")
            ]
        );
        assert_eq!(log[1].header("authorization"), Some("Bearer t"));
        assert_eq!(log[1].header("cookie"), None);
        assert_eq!(log[1].status, 200);
    }

    #[test]
    fn pattern_hash_matches_the_bytes_and_versions_differ_everywhere() {
        let size = (1 << 20) + 17;
        let body = pattern(1, size);
        assert_eq!(pattern_sha256(1, size), hex::encode(Sha256::digest(&body)));
        let second = pattern(2, size);
        assert!(body
            .iter()
            .zip(&second)
            .all(|(first, second)| first != second));
    }

    #[test]
    fn single_ranges_answer_206_and_unsatisfiable_ones_416() {
        let server = Server::start(config(1000));
        let mut connection = connect(&server);
        let body = pattern(1, 1000);
        for (spec, start, end) in [
            ("bytes=10-19", 10, 19),
            ("bytes=990-", 990, 999),
            ("bytes=-5", 995, 999),
            ("bytes=995-5000", 995, 999),
        ] {
            let answer = request(&mut connection, "GET", "/", &[&format!("Range: {spec}")]);
            assert_eq!(answer.status, 206, "{spec}");
            assert_eq!(
                answer.header("Content-Range"),
                Some(format!("bytes {start}-{end}/1000").as_str())
            );
            assert_eq!(answer.body, body[start..=end], "{spec}");
        }
        for spec in ["bytes=1000-", "bytes=2000-3000", "bytes=-0"] {
            let answer = request(&mut connection, "GET", "/", &[&format!("Range: {spec}")]);
            assert_eq!(answer.status, 416, "{spec}");
            assert_eq!(answer.header("Content-Range"), Some("bytes */1000"));
        }
        let multiple = request(&mut connection, "GET", "/", &["Range: bytes=0-1,5-6"]);
        assert_eq!((multiple.status, multiple.body.len()), (200, 1000));
        let log = server.log();
        assert_eq!(log[0].range.as_deref(), Some("bytes=10-19"));
        assert_eq!(log[4].status, 416);
    }

    #[test]
    fn ignored_ranges_answer_the_full_body() {
        let server = Server::start(Config {
            ranges: RangeSupport::Ignore,
            ..config(1000)
        });
        let answer = request(&mut connect(&server), "GET", "/", &["Range: bytes=10-19"]);
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, pattern(1, 1000));
        assert_eq!(answer.header("Accept-Ranges"), Some("bytes"));
    }

    #[test]
    fn body_bytes_sent_counts_the_body_of_every_get_and_nothing_of_a_head() {
        let server = Server::start(config(1000));
        let mut connection = connect(&server);
        request(&mut connection, "HEAD", "/", &[]);
        request(&mut connection, "GET", "/", &["Range: bytes=0-9"]);
        request(&mut connection, "GET", "/", &[]);
        // The server counts a chunk after it has written it, so the client may see the last
        // answer first; a closed connection and an idle server leave the count final.
        drop(connection);
        server.wait_idle();
        assert_eq!(server.body_bytes_sent(), 1010);
    }

    #[test]
    fn head_and_get_answers_can_disagree_or_leave_out_their_framing() {
        let server = Server::start(Config {
            head_etag: Some("\"head\"".into()),
            ..config(100)
        });
        let mut connection = connect(&server);
        assert_eq!(
            request(&mut connection, "HEAD", "/", &[]).header("ETag"),
            Some("\"head\"")
        );
        assert_eq!(
            request(&mut connection, "GET", "/", &[]).header("ETag"),
            Some("\"v1\"")
        );

        let bare = Server::start(Config {
            head_bare: true,
            ..config(100)
        });
        let answer = request(&mut connect(&bare), "HEAD", "/", &[]);
        assert_eq!(answer.status, 200);
        assert_eq!(answer.header("Content-Length"), None);
        assert_eq!(answer.header("ETag"), None);
        assert_eq!(answer.header("Last-Modified"), None);

        let unframed = Server::start(Config {
            get_length_header: false,
            ..config(100)
        });
        let mut connection = connect(&unframed);
        let (status, headers) = {
            connection
                .get_mut()
                .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
                .unwrap();
            read_head(&mut connection).unwrap()
        };
        assert_eq!(status, 200);
        assert!(headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
        let mut body = Vec::new();
        connection.read_to_end(&mut body).unwrap();
        assert_eq!(body, pattern(1, 100));
    }

    #[test]
    fn if_range_must_match_a_strong_etag_or_the_last_modified_date() {
        let server = Server::start(config(100));
        let mut connection = connect(&server);
        let date = request(&mut connection, "HEAD", "/", &[])
            .header("Last-Modified")
            .unwrap()
            .to_owned();
        for (validator, status) in [
            ("\"v1\"", 206),
            ("\"other\"", 200),
            ("W/\"v1\"", 200),
            (date.as_str(), 206),
            ("Sun, 31 Dec 2023 00:00:00 GMT", 200),
        ] {
            let answer = request(
                &mut connection,
                "GET",
                "/",
                &["Range: bytes=10-19", &format!("If-Range: {validator}")],
            );
            assert_eq!(answer.status, status, "{validator}");
        }
        assert_eq!(server.log()[1].if_range.as_deref(), Some("\"v1\""));

        let weak = Server::start(Config {
            etag: EtagKind::Weak,
            ..config(100)
        });
        let answer = request(
            &mut connect(&weak),
            "GET",
            "/",
            &["Range: bytes=10-19", "If-Range: W/\"v1\""],
        );
        assert_eq!(answer.status, 200);
    }

    #[test]
    fn advertised_ranges_head_length_and_head_support_are_independent_of_range_service() {
        let server = Server::start(Config {
            accept_ranges_header: false,
            head_content_length: Some(7),
            ..config(1000)
        });
        let mut connection = connect(&server);
        let head = request(&mut connection, "HEAD", "/", &[]);
        assert_eq!(head.header("Content-Length"), Some("7"));
        assert_eq!(head.header("Accept-Ranges"), None);
        let part = request(&mut connection, "GET", "/", &["Range: bytes=0-9"]);
        assert_eq!((part.status, part.body.len()), (206, 10));
        assert_eq!(part.header("Accept-Ranges"), None);
        assert_eq!(request(&mut connection, "GET", "/", &[]).body.len(), 1000);

        let refusing = Server::start(Config {
            head_not_allowed: true,
            ..config(1000)
        });
        let mut connection = connect(&refusing);
        assert_eq!(request(&mut connection, "HEAD", "/", &[]).status, 405);
        assert_eq!(request(&mut connection, "GET", "/", &[]).status, 200);
    }

    #[test]
    fn validators_follow_the_configuration_and_the_body_version() {
        let weak = Server::start(Config {
            etag: EtagKind::Weak,
            ..config(10)
        });
        assert_eq!(
            request(&mut connect(&weak), "HEAD", "/", &[]).header("ETag"),
            Some("W/\"v1\"")
        );

        let bare = Server::start(Config {
            etag: EtagKind::Absent,
            last_modified: false,
            ..config(10)
        });
        let answer = request(&mut connect(&bare), "HEAD", "/", &[]);
        assert_eq!(answer.header("ETag"), None);
        assert_eq!(answer.header("Last-Modified"), None);

        let varying = Server::start(Config {
            vary_etag: true,
            ..config(10)
        });
        let mut connection = connect(&varying);
        let first = request(&mut connection, "HEAD", "/", &[]);
        let second = request(&mut connection, "HEAD", "/", &[]);
        assert_ne!(first.header("ETag"), second.header("ETag"));

        let switching = Server::start(Config {
            switch_version_after: Some(2),
            ..config(64)
        });
        let mut connection = connect(&switching);
        let before = request(&mut connection, "GET", "/", &[]);
        request(&mut connection, "HEAD", "/", &[]);
        let after = request(&mut connection, "GET", "/", &[]);
        assert_eq!(before.body, pattern(1, 64));
        assert_eq!(after.body, pattern(2, 64));
        assert_ne!(before.header("ETag"), after.header("ETag"));
        assert_ne!(
            before.header("Last-Modified"),
            after.header("Last-Modified")
        );
    }

    #[test]
    fn throttles_every_connection_and_a_chosen_slow_one() {
        // Sleeping only lengthens a transfer, so only lower bounds are asserted.
        let timed = |connection: &mut BufReader<TcpStream>| {
            let began = Instant::now();
            assert_eq!(request(connection, "GET", "/", &[]).body.len(), 16 * 1024);
            began.elapsed()
        };
        let throttled = Server::start(Config {
            throttle: Some(32 * 1024),
            ..config(16 * 1024)
        });
        assert!(timed(&mut connect(&throttled)) >= Duration::from_millis(450));

        // Only a connection that starts with a GET whose range begins above 0 can be slow, and
        // the index counts those connections alone.
        let one_slow = Server::start(Config {
            slow_connection: Some((1, 32 * 1024)),
            ..config(64 * 1024)
        });
        request(&mut connect(&one_slow), "HEAD", "/", &[]);
        request(&mut connect(&one_slow), "GET", "/", &["Range: bytes=0-0"]);
        let mut first_ranged = connect(&one_slow);
        request(&mut first_ranged, "GET", "/", &["Range: bytes=1-10"]);
        let mut second_ranged = connect(&one_slow);
        let began = Instant::now();
        let slow = request(&mut second_ranged, "GET", "/", &["Range: bytes=100-16483"]);
        assert_eq!(slow.body.len(), 16 * 1024);
        assert!(began.elapsed() >= Duration::from_millis(450));
        assert_eq!(one_slow.log()[3].connection, 3);
    }

    #[test]
    fn a_refused_connection_does_not_take_the_slow_slot() {
        let server = Server::start(Config {
            slow_connection: Some((0, 32 * 1024)),
            max_connections: Some(1),
            ..config(64 * 1024)
        });
        // Admitted first, but it sends its first request after the refused one.
        let mut admitted = connect(&server);
        let refused = request(&mut connect(&server), "GET", "/", &["Range: bytes=1-10"]);
        assert_eq!(refused.status, 503);
        let began = Instant::now();
        let slow = request(&mut admitted, "GET", "/", &["Range: bytes=1-16384"]);
        assert_eq!(slow.body.len(), 16 * 1024);
        assert!(began.elapsed() >= Duration::from_millis(450));
    }

    /// Sends a GET for `range` and reads the head of the answer.
    fn start_ranged_get(connection: &mut BufReader<TcpStream>, range: &str) {
        connection
            .get_mut()
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: test\r\nRange: bytes={range}\r\n\r\n").as_bytes(),
            )
            .unwrap();
        assert_eq!(read_head(connection).unwrap().0, 206);
    }

    fn is_silent(connection: &mut BufReader<TcpStream>) -> bool {
        connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let silent = connection.fill_buf().is_err();
        connection
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        silent
    }

    #[test]
    fn a_stalled_body_goes_on_when_a_request_takes_a_range_inside_it() {
        let server = Server::start(Config {
            stall: Some((0, 10)),
            ..config(1000)
        });
        let body = pattern(1, 1000);
        let mut stalled = connect(&server);
        start_ranged_get(&mut stalled, "100-399");
        let mut first = [0; 10];
        stalled.read_exact(&mut first).unwrap();
        assert_eq!(first, body[100..110]);
        assert!(is_silent(&mut stalled));

        // Only the first connection that starts with a range above 0 stalls, and a range that
        // starts outside the body does not release it.
        request(&mut connect(&server), "GET", "/", &["Range: bytes=500-599"]);
        assert!(is_silent(&mut stalled));
        request(&mut connect(&server), "GET", "/", &["Range: bytes=300-399"]);
        let mut rest = vec![0; 290];
        stalled.read_exact(&mut rest).unwrap();
        assert_eq!(rest, body[110..400]);
    }

    #[test]
    fn a_stalled_body_ends_when_the_client_closes_the_connection() {
        let server = Server::start(Config {
            stall: Some((0, 10)),
            ..config(1000)
        });
        let mut stalled = connect(&server);
        start_ranged_get(&mut stalled, "100-399");
        let mut first = [0; 10];
        stalled.read_exact(&mut first).unwrap();
        drop(stalled);
        let began = Instant::now();
        server.wait_idle();
        assert!(began.elapsed() < STALL_LIMIT / 2);
    }

    #[test]
    fn a_held_body_stops_where_the_part_taken_starts_and_ends_when_the_client_closes() {
        let server = Server::start(Config {
            stall: Some((0, 10)),
            hold_at_takeover: true,
            ..config(1000)
        });
        let body = pattern(1, 1000);
        let mut held = connect(&server);
        start_ranged_get(&mut held, "100-399");
        let mut first = [0; 10];
        held.read_exact(&mut first).unwrap();
        assert!(is_silent(&mut held));

        request(&mut connect(&server), "GET", "/", &["Range: bytes=300-399"]);
        let mut rest = vec![0; 190];
        held.read_exact(&mut rest).unwrap();
        assert_eq!(rest, body[110..300]);
        assert!(is_silent(&mut held));
        assert_eq!(server.abandoned_holds(), 0);

        drop(held);
        let began = Instant::now();
        server.wait_idle();
        assert!(began.elapsed() < STALL_LIMIT / 2);
        assert_eq!(server.abandoned_holds(), 1);
    }

    #[test]
    fn wait_idle_returns_after_a_request_that_was_sent_and_abandoned_is_logged() {
        let server = Server::start(config(10));
        let mut connection = connect(&server);
        connection
            .get_mut()
            .write_all(
                b"GET /late HTTP/1.1
Host: test

",
            )
            .unwrap();
        drop(connection);
        server.wait_idle();
        assert_eq!(
            server
                .log()
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["/late"]
        );
    }

    #[test]
    fn connections_beyond_the_maximum_get_503_with_retry_after() {
        for (retry_after, expected) in [
            (None, None),
            (Some(RetryAfter::Seconds(7)), Some("7")),
            (
                Some(RetryAfter::HttpDate("Wed, 01 Jan 2031 00:00:00 GMT".into())),
                Some("Wed, 01 Jan 2031 00:00:00 GMT"),
            ),
        ] {
            let server = Server::start(Config {
                max_connections: Some(1),
                over_limit: OverLimit::Unavailable(retry_after),
                ..config(10)
            });
            let mut admitted = connect(&server);
            assert_eq!(request(&mut admitted, "GET", "/", &[]).status, 200);
            let refused = request(&mut connect(&server), "GET", "/", &[]);
            assert_eq!(refused.status, 503);
            assert_eq!(refused.header("Retry-After"), expected);
            assert_eq!(request(&mut admitted, "GET", "/", &[]).status, 200);
        }
    }

    #[test]
    fn connections_beyond_the_maximum_can_be_reset() {
        let server = Server::start(Config {
            max_connections: Some(1),
            over_limit: OverLimit::Reset,
            ..config(10)
        });
        let mut admitted = connect(&server);
        assert_eq!(request(&mut admitted, "GET", "/", &[]).status, 200);
        assert!(try_request(&mut connect(&server), "GET", "/", &[]).is_none());
        assert_eq!(request(&mut admitted, "GET", "/", &[]).status, 200);
        assert_eq!(server.log().len(), 2);
    }

    #[test]
    fn the_first_responses_are_cut_after_the_configured_body_bytes() {
        let server = Server::start(Config {
            cut_body: Some(CutBody {
                after_bytes: 100,
                responses: 2,
            }),
            ..config(1000)
        });
        // A response that is not longer than the cut point is never cut and uses no slot.
        let short = request(&mut connect(&server), "GET", "/", &["Range: bytes=0-49"]);
        assert_eq!(short.body, pattern(1, 1000)[..50]);
        for _ in 0..2 {
            let mut connection = connect(&server);
            let (status, _) = {
                connection
                    .get_mut()
                    .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
                    .unwrap();
                read_head(&mut connection).unwrap()
            };
            assert_eq!(status, 200);
            let mut body = Vec::new();
            connection.read_to_end(&mut body).unwrap();
            assert_eq!(body, pattern(1, 1000)[..100]);
        }
        let full = request(&mut connect(&server), "GET", "/", &[]);
        assert_eq!(full.body, pattern(1, 1000));
        assert_eq!(
            server
                .log()
                .iter()
                .map(|entry| entry.body_cut)
                .collect::<Vec<_>>(),
            [false, true, true, false]
        );
    }

    #[test]
    fn scripted_answers_come_first_then_normal_service_resumes() {
        let server = Server::start(Config {
            scripted: vec![
                Scripted {
                    status: 429,
                    retry_after: Some(RetryAfter::Seconds(3)),
                },
                Scripted {
                    status: 502,
                    retry_after: None,
                },
            ],
            ..config(10)
        });
        let mut connection = connect(&server);
        let throttled = request(&mut connection, "GET", "/", &[]);
        assert_eq!(throttled.status, 429);
        assert_eq!(throttled.header("Retry-After"), Some("3"));
        let bad_gateway = request(&mut connection, "HEAD", "/", &[]);
        assert_eq!(bad_gateway.status, 502);
        assert_eq!(bad_gateway.header("Retry-After"), None);
        assert_eq!(request(&mut connection, "GET", "/", &[]).status, 200);
    }

    #[test]
    fn a_redirect_target_expires_and_the_source_points_to_a_fresh_one() {
        let server = Server::start(Config {
            redirect: Some(Redirect {
                from: "/file".into(),
                target: "/old".into(),
                fresh_target: "/new".into(),
                target_requests_before_403: 2,
            }),
            ..config(10)
        });
        let mut connection = connect(&server);
        let location = |answer: &Answer| answer.header("Location").map(str::to_owned);
        let first = request(&mut connection, "GET", "/file", &[]);
        assert_eq!(
            (first.status, location(&first).as_deref()),
            (302, Some("/old"))
        );
        assert_eq!(request(&mut connection, "GET", "/old", &[]).status, 200);
        let again = request(&mut connection, "GET", "/file", &[]);
        assert_eq!(location(&again).as_deref(), Some("/old"));
        assert_eq!(request(&mut connection, "HEAD", "/old", &[]).status, 200);
        assert_eq!(request(&mut connection, "GET", "/old", &[]).status, 403);
        let fresh = request(&mut connection, "GET", "/file", &[]);
        assert_eq!(
            (fresh.status, location(&fresh).as_deref()),
            (302, Some("/new"))
        );
        assert_eq!(
            request(&mut connection, "GET", "/new", &[]).body,
            pattern(1, 10)
        );
    }
}
