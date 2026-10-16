//! Tool processes: capture runs, the running download monitor and bounded output readers.

use super::*;

pub(super) struct Captured {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: String,
}
pub(super) fn run_capture(
    executable: &Path,
    args: &[String],
    input: &[u8],
    timeout: Duration,
    control: &TransferControl,
) -> Result<Captured> {
    run_capture_with_proxy(executable, args, input, timeout, control, None)
}

pub(super) fn run_ytdlp_capture(
    executable: &Path,
    args: &[String],
    input: &mut Vec<u8>,
    redactions: &mut Vec<String>,
    timeout: Duration,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<Captured> {
    let proxy = ExternalProxy::spawn_for_inspection(network.clone(), control.clone())?;
    redactions.push(proxy.proxy_url().to_string());
    let input_len = input.len();
    let configured = proxy.append_ytdlp_configuration(input);
    if let Err(error) = configured {
        input.truncate(input_len);
        return Err(error);
    }
    let result = run_capture_with_proxy(executable, args, input, timeout, control, Some(&proxy));
    input.truncate(input_len);
    result
}

/// Starts a suspended tool process and puts it under an owned job object before it runs.
/// Failure to start or to own the process carries the stable tool-start code.
fn spawn_owned(command: &mut Command, executable: &Path) -> Result<(Child, ProcessJob)> {
    let mut child = command.spawn().map_err(|error| {
        crate::error_codes::coded(
            crate::error_codes::TOL_003,
            crate::i18n::ui_owned!(
                format!("{} başlatılamadı: {error}", executable.display()),
                format!("{} could not be started: {error}", executable.display())
            ),
        )
    })?;
    match ProcessJob::assign(&child) {
        Ok(job) => Ok((child, job)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(crate::error_codes::coded(
                crate::error_codes::TOL_003,
                format!("{error:#}"),
            ))
        }
    }
}

pub(super) fn run_capture_with_proxy(
    executable: &Path,
    args: &[String],
    input: &[u8],
    timeout: Duration,
    control: &TransferControl,
    proxy: Option<&ExternalProxy>,
) -> Result<Captured> {
    let mut command = Command::new(executable);
    if let Some(proxy) = proxy.as_ref() {
        proxy.apply_environment(&mut command);
    }
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let (mut child, job) = spawn_owned(&mut command, executable)?;
    let input_thread = write_input(&mut child, input)?;
    let stdout = child.stdout.take().ok_or_else(|| {
        anyhow!(crate::i18n::ui(
            "yt-dlp stdout alınamadı",
            "Could not capture yt-dlp stdout"
        ))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        anyhow!(crate::i18n::ui(
            "yt-dlp stderr alınamadı",
            "Could not capture yt-dlp stderr"
        ))
    })?;
    let out_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout
            .take((MAX_INSPECT_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes);
        bytes
    });
    let err_thread = thread::spawn(move || read_bounded(&mut stderr, MAX_ERROR_BYTES));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout || control.stop_requested() {
            job.terminate();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            let _ = input_thread.join();
            if control.stop_requested() {
                bail!(crate::i18n::ui(
                    "Kaynak hazırlığı durduruldu",
                    "Source preparation was stopped"
                ));
            }
            crate::bail_code!(
                crate::error_codes::MED_011,
                "{}",
                crate::i18n::ui_owned!(
                    format!(
                        "Medya analizi {} saniye içinde tamamlanmadı",
                        timeout.as_secs()
                    ),
                    format!(
                        "The media analysis did not finish within {} seconds",
                        timeout.as_secs()
                    )
                )
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    // Whatever still runs now is an orphan holding the pipes; the readers would wait on it.
    job.terminate();
    let input_result = input_thread.join().unwrap_or_else(|_| {
        Err(std::io::Error::other(crate::i18n::ui(
            "Medya girdi işçisi durdu",
            "The media input worker stopped",
        )))
    });
    if status.success() {
        input_result.context(crate::i18n::ui(
            "Medya isteği gönderilemedi",
            "Could not send the media request",
        ))?;
    }
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

pub(super) struct RunningDownload {
    pub(super) overflow: Arc<std::sync::atomic::AtomicBool>,
    pub(super) child: Child,
    pub(super) job: ProcessJob,
    pub(super) lines: mpsc::Receiver<String>,
    pub(super) stdin_thread: Option<thread::JoinHandle<std::io::Result<()>>>,
    pub(super) input_error: Option<std::io::Error>,
    pub(super) stdout_thread: Option<thread::JoinHandle<()>>,
    pub(super) stderr: Arc<Mutex<BoundedLines>>,
    /// Last time the child said anything on stderr, shared with the stall watchdog.
    pub(super) stderr_alive: Arc<std::sync::atomic::AtomicU64>,
    pub(super) stderr_thread: Option<thread::JoinHandle<()>>,
    // Drops after the owned process job and child so its loopback sockets remain
    // governed until every owned descendant has stopped.
    pub(super) _proxy: Option<ExternalProxy>,
}
impl RunningDownload {
    pub(super) fn spawn_ytdlp(
        executable: &Path,
        args: &[String],
        input: &mut Vec<u8>,
        redactions: &mut Vec<String>,
        control: &TransferControl,
        network: &NetworkGovernor,
        live: bool,
    ) -> Result<Self> {
        let proxy = if live {
            ExternalProxy::spawn_for_live_recording(network.clone(), control.clone())?
        } else {
            ExternalProxy::spawn(network.clone(), control.clone())?
        };
        redactions.push(proxy.proxy_url().to_string());
        let input_len = input.len();
        let configured = proxy.append_ytdlp_configuration(input);
        if let Err(error) = configured {
            input.truncate(input_len);
            return Err(error);
        }
        let result = Self::spawn_with_proxy(executable, args, input, Some(proxy));
        input.truncate(input_len);
        result
    }

    pub(super) fn spawn_with_proxy(
        executable: &Path,
        args: &[String],
        input: &[u8],
        proxy: Option<ExternalProxy>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        if let Some(proxy) = proxy.as_ref() {
            proxy.apply_environment(&mut command);
        }
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        let (mut child, job) = spawn_owned(&mut command, executable)?;
        let stdin_thread = Some(write_input(&mut child, input)?);
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "yt-dlp stdout alınamadı",
                "Could not capture yt-dlp stdout"
            ))
        })?;
        let stderr_pipe = child.stderr.take().ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "yt-dlp stderr alınamadı",
                "Could not capture yt-dlp stderr"
            ))
        })?;
        let (sender, lines) = mpsc::sync_channel(256);
        let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_overflow = overflow.clone();
        let stdout_thread = thread::spawn(move || {
            for line in bounded_lines(stdout, MAX_ERROR_BYTES) {
                match line {
                    Ok(line) => {
                        if line_was_truncated(&line) {
                            reader_overflow.store(true, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                        if line.starts_with(FILE_PREFIX) {
                            if sender.try_send(line).is_err() {
                                reader_overflow.store(true, std::sync::atomic::Ordering::Release);
                            }
                        } else {
                            let _ = sender.try_send(line);
                        }
                    }
                    Err(_) => {
                        reader_overflow.store(true, std::sync::atomic::Ordering::Release);
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(BoundedLines::new(MAX_ERROR_BYTES)));
        let stderr_for_thread = Arc::clone(&stderr);
        let stderr_alive = Arc::new(std::sync::atomic::AtomicU64::new(
            crate::network::now_epoch_ms(),
        ));
        let alive_for_thread = Arc::clone(&stderr_alive);
        let stderr_thread = thread::spawn(move || {
            for line in bounded_lines(stderr_pipe, MAX_ERROR_BYTES).map_while(Result::ok) {
                alive_for_thread.store(
                    crate::network::now_epoch_ms(),
                    std::sync::atomic::Ordering::Release,
                );
                let mut log = stderr_for_thread.lock().unwrap_or_else(|e| e.into_inner());
                log.push(line);
            }
        });
        Ok(Self {
            overflow,
            child,
            job,
            lines,
            stdin_thread,
            input_error: None,
            stdout_thread: Some(stdout_thread),
            stderr,
            stderr_alive,
            stderr_thread: Some(stderr_thread),
            _proxy: proxy,
        })
    }
    pub(super) fn terminate(&mut self) {
        self.job.terminate();
        let _ = self.child.wait();
        self.join_readers();
    }
    pub(super) fn join_readers(&mut self) {
        if let Some(thread) = self.stdin_thread.take() {
            self.input_error = thread
                .join()
                .unwrap_or_else(|_| {
                    Err(std::io::Error::other(crate::i18n::ui(
                        "Medya girdi işçisi durdu",
                        "The media input worker stopped",
                    )))
                })
                .err();
        }
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
    pub(super) fn stderr_text(&self) -> String {
        self.stderr.lock().map(|v| v.text()).unwrap_or_default()
    }
}
impl Drop for RunningDownload {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.job.terminate();
            let _ = self.child.wait();
        }
        self.join_readers();
    }
}

/// Tracks whether a transfer still produces output. A tunnel that died mid-stream leaves the
/// downloader sleeping between its own retries with no lines and no bytes for minutes.
/// Progress-template mode can suppress yt-dlp's retry chatter, so liveness is "any signal":
/// stdout lines, the child's stderr, or a fresh outbound connection attempt through the
/// per-child proxy. A healthy fragment-retry backoff must never look like a dead tunnel.
pub(super) struct ActivityWatch {
    pub(super) last: Instant,
    pub(super) limit: Duration,
    pub(super) pings: Vec<Arc<std::sync::atomic::AtomicU64>>,
    /// A yt-dlp post-processor (FFmpeg merge, remux or audio conversion) is running.
    pub(super) postprocessing: bool,
}

impl ActivityWatch {
    pub(super) fn new(limit: Duration, pings: Vec<Arc<std::sync::atomic::AtomicU64>>) -> Self {
        Self {
            last: Instant::now(),
            limit,
            pings,
            postprocessing: false,
        }
    }

    pub(super) fn touch(&mut self) {
        self.last = Instant::now();
    }

    /// yt-dlp reports a post-processor only at its start and end. FFmpeg runs silently and
    /// opens no connection for as long as the local work takes, so it gets the longer
    /// post-processing limit instead of the transfer limit.
    pub(super) fn observe(&mut self, line: &str) {
        if let Some(status) = line.strip_prefix(POSTPROCESS_PREFIX) {
            self.postprocessing = matches!(status.trim(), "started" | "processing");
        }
    }

    pub(super) fn stalled(&self) -> bool {
        let limit = if self.postprocessing {
            POSTPROCESS_STALL_LIMIT
        } else {
            self.limit
        };
        if self.last.elapsed() < limit {
            return false;
        }
        let now = crate::network::now_epoch_ms();
        let limit_ms = limit.as_millis() as u64;
        self.pings.iter().all(|ping| {
            now.saturating_sub(ping.load(std::sync::atomic::Ordering::Acquire)) >= limit_ms
        })
    }
}

/// yt-dlp hands a live stream to FFmpeg, which reports no progress, so the size of the growing
/// `.part` files in the workspace is the only measure of what has been recorded.
pub(super) struct LiveRecordingGauge {
    workspace: PathBuf,
    sampled_at: Option<Instant>,
    /// The size each time it changed, with when the change was first seen.
    steps: VecDeque<(u64, Instant)>,
}

impl LiveRecordingGauge {
    const INTERVAL: Duration = Duration::from_secs(1);
    /// How far back the speed averages the size changes.
    const SPEED_WINDOW: Duration = Duration::from_secs(5);

    pub(super) fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            sampled_at: None,
            steps: VecDeque::new(),
        }
    }

    /// At most once per interval: the recorded size on top of `base` and the speed between
    /// its recent changes. The file grows in steps, a segment or a write buffer at a time,
    /// up to many seconds apart at a low bitrate; the speed holds between two steps, falls
    /// once the next step is later than the last gap and is zero once it is far overdue.
    pub(super) fn sample(&mut self, base: u64, now: Instant) -> Option<TransferProgress> {
        if self
            .sampled_at
            .is_some_and(|at| now.duration_since(at) < Self::INTERVAL)
        {
            return None;
        }
        self.sampled_at = Some(now);
        let bytes = part_file_bytes(&self.workspace);
        if self.steps.back().is_none_or(|(last, _)| *last != bytes) {
            self.steps.push_back((bytes, now));
        }
        while self.steps.len() > 2 && now.duration_since(self.steps[1].1) >= Self::SPEED_WINDOW {
            self.steps.pop_front();
        }
        let (first, from) = self.steps[0];
        let (last, to) = self.steps[self.steps.len() - 1];
        let gap = to.duration_since(from);
        let quiet = now.duration_since(to);
        let seconds = gap.max(quiet).as_secs_f64();
        let speed = if seconds > 0.0 && quiet <= gap.max(Self::SPEED_WINDOW) * 2 {
            (last.saturating_sub(first) as f64 / seconds) as u64
        } else {
            0
        };
        Some(TransferProgress {
            downloaded: base.saturating_add(bytes),
            total: None,
            speed,
            eta: None,
            phase: crate::i18n::ui("Medya indiriliyor", "Downloading media").into(),
        })
    }
}

fn part_file_bytes(directory: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(directory) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("part"))
        })
        // fs::metadata opens the file; the directory entry (DirEntry::metadata) keeps the size
        // from when FFmpeg opened it until FFmpeg closes it.
        .filter_map(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .fold(0u64, u64::saturating_add)
}

pub(super) enum MonitorOutcome {
    RestartForSpeed,
    Stalled,
    Paused,
    Cancelled,
    /// A live recording was ended by the user to keep what it has recorded.
    Finished,
    Exited(ExitStatus),
}
pub(super) fn monitor_download(
    running: &mut RunningDownload,
    control: &TransferControl,
    launch_limit: u64,
    progress: &mut dyn FnMut(TransferProgress),
    completed_files: &mut HashSet<PathBuf>,
    last_path: &mut Option<PathBuf>,
    mut live: Option<&mut LiveRecordingGauge>,
) -> Result<MonitorOutcome> {
    let mut pending_limit: Option<(u64, Instant)> = None;
    let mut seen_bytes = 0u64;
    let mut pings = vec![Arc::clone(&running.stderr_alive)];
    if let Some(proxy) = running._proxy.as_ref() {
        pings.push(proxy.outbound_ping());
    }
    let mut activity = ActivityWatch::new(STALL_LIMIT, pings);
    let mut live_bytes = None;
    loop {
        let mut active = false;
        while let Ok(line) = running.lines.try_recv() {
            active = true;
            activity.observe(&line);
            // A live recording's size comes from the gauge alone; the tool's own counter
            // covers one stream and would alternate with it.
            if live.is_some() && line.starts_with(PROGRESS_PREFIX) {
                continue;
            }
            handle_machine_line(&line, progress, completed_files, last_path, &mut seen_bytes);
        }
        if active {
            activity.touch();
        }
        if let Some(gauge) = live.as_deref_mut() {
            if let Some(update) =
                gauge.sample(completed_file_bytes(completed_files), Instant::now())
            {
                if live_bytes != Some(update.downloaded) {
                    live_bytes = Some(update.downloaded);
                    activity.touch();
                }
                progress(update);
            }
        }
        if control.is_cancelled() {
            running.terminate();
            return Ok(MonitorOutcome::Cancelled);
        }
        if control.finish_requested() {
            running.terminate();
            return Ok(MonitorOutcome::Finished);
        }
        if control.is_paused() {
            running.terminate();
            return Ok(MonitorOutcome::Paused);
        }
        let requested_limit = control.speed_limit();
        if requested_limit == launch_limit {
            pending_limit = None;
        } else {
            match pending_limit {
                Some((value, since))
                    if value == requested_limit
                        && since.elapsed() >= Duration::from_millis(750) =>
                {
                    running.terminate();
                    return Ok(MonitorOutcome::RestartForSpeed);
                }
                Some((value, _)) if value == requested_limit => {}
                _ => pending_limit = Some((requested_limit, Instant::now())),
            }
        }
        // A tunnel that died mid-stream leaves yt-dlp sleeping between retries with no bytes
        // moving for minutes; reconnect the transfer instead of waiting it out.
        if activity.stalled() {
            running.terminate();
            return Ok(MonitorOutcome::Stalled);
        }
        if let Some(status) = running.child.try_wait()? {
            while let Ok(line) = running.lines.try_recv() {
                handle_machine_line(&line, progress, completed_files, last_path, &mut seen_bytes);
            }
            // The tool has exited: anything still alive is an orphan holding the output pipes.
            running.job.terminate();
            running.join_readers();
            while let Ok(line) = running.lines.try_recv() {
                handle_machine_line(&line, progress, completed_files, last_path, &mut seen_bytes);
            }
            if running.overflow.load(std::sync::atomic::Ordering::Acquire) {
                bail!(crate::i18n::ui("Medya çıktı kuyruğu sınırı aşıldı; eksik playlist yayınlanmadı", "The media output queue limit was exceeded; the incomplete playlist was not published"))
            }
            return Ok(MonitorOutcome::Exited(status));
        }
        match running.lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                activity.observe(&line);
                handle_machine_line(&line, progress, completed_files, last_path, &mut seen_bytes);
                activity.touch();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

pub(super) fn handle_machine_line(
    line: &str,
    progress: &mut dyn FnMut(TransferProgress),
    completed_files: &mut HashSet<PathBuf>,
    last_path: &mut Option<PathBuf>,
    seen_bytes: &mut u64,
) {
    if let Some(fields) = line.strip_prefix(PROGRESS_PREFIX) {
        let values: Vec<_> = fields.split('\t').collect();
        if values.len() >= 5 {
            let base = completed_file_bytes(completed_files);
            let current = parse_number(values[1]).unwrap_or(0);
            *seen_bytes = (*seen_bytes).max(base.saturating_add(current));
            let total = parse_number(values[2]).map(|v| base.saturating_add(v));
            let status = values[0];
            progress(TransferProgress {
                downloaded: base.saturating_add(current),
                total,
                speed: parse_number(values[3]).unwrap_or(0),
                eta: parse_number(values[4]),
                phase: if status == "finished" {
                    crate::i18n::ui("Medya işleniyor", "Processing media").into()
                } else {
                    crate::i18n::ui("Medya indiriliyor", "Downloading media").into()
                },
            });
        }
    } else if line.starts_with(POSTPROCESS_PREFIX) {
        progress(TransferProgress {
            downloaded: completed_file_bytes(completed_files),
            total: None,
            speed: 0,
            eta: None,
            phase: crate::i18n::ui("FFmpeg ile işleniyor", "Processing with FFmpeg").into(),
        });
    } else if let Some(json_path) = line.strip_prefix(FILE_PREFIX) {
        if let Ok(path) = serde_json::from_str::<String>(json_path) {
            let path = PathBuf::from(path);
            if path.is_file() {
                completed_files.insert(path.clone());
            }
            *last_path = Some(path);
        }
    }
}

pub(super) fn parse_number(value: &str) -> Option<u64> {
    let value = value.trim();
    if matches!(value, "" | "NA" | "None" | "null" | "unknown") {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64)
}
pub(super) fn completed_file_bytes(files: &HashSet<PathBuf>) -> u64 {
    files
        .iter()
        .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
        .fold(0u64, u64::saturating_add)
}
pub(super) fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::metadata(path).with_context(|| {
        crate::i18n::ui_owned!(
            format!("Tamamlanan medya bulunamadı: {}", path.display()),
            format!("The completed media was not found: {}", path.display())
        )
    })?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let mut total = 0u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                pending.push(entry.path());
            } else if ty.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

pub(super) struct BoundedLines {
    pub(super) lines: VecDeque<String>,
    pub(super) bytes: usize,
    pub(super) limit: usize,
}
impl BoundedLines {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }
    pub(super) fn push(&mut self, line: String) {
        let line = bounded_tail(&line, self.limit.saturating_sub(1));
        self.bytes = self.bytes.saturating_add(line.len() + 1);
        self.lines.push_back(line);
        while self.bytes > self.limit && self.lines.len() > 1 {
            if let Some(line) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(line.len() + 1);
            }
        }
    }
    pub(super) fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}
pub(super) fn read_bounded(reader: &mut dyn Read, limit: usize) -> String {
    let mut log = BoundedLines::new(limit);
    for line in bounded_lines(reader, limit).map_while(Result::ok) {
        log.push(line);
    }
    log.text()
}

pub(super) fn useful_error(stderr: &str, secrets: &[String]) -> String {
    let mut result = stderr
        .replace("Reading options from STDIN - EOF (Ctrl+Z) to end:", "")
        .trim()
        .to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        result = result.replace(secret, "[gizlendi]");
    }
    // Generic extractor IDs may be a signed URL query or an opaque CDN path.
    // The actual failure after the ID is useful; repeating that ID can expose tokens.
    result = result
        .lines()
        .map(|line| {
            if let Some(detail) = line.trim_start().strip_prefix("ERROR: [generic] ") {
                if let Some((identifier, failure)) = detail.split_once(": ") {
                    if !identifier.is_empty() && !identifier.chars().any(char::is_whitespace) {
                        return format!("ERROR: [generic] {failure}");
                    }
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    result = redact_url_credentials(&result);
    if result.is_empty() {
        crate::i18n::ui(
            "yt-dlp ayrıntı vermeden başarısız oldu",
            "yt-dlp failed without giving details",
        )
        .into()
    } else {
        bounded_tail(&result, MAX_ERROR_BYTES)
    }
}
pub(super) fn redact_url_credentials(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            let trimmed =
                token.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | ','));
            if let Ok(mut url) = Url::parse(trimmed) {
                if matches!(url.scheme(), "http" | "https") {
                    if !url.username().is_empty() {
                        let _ = url.set_username("redacted");
                    }
                    if url.password().is_some() {
                        let _ = url.set_password(Some("redacted"));
                    }
                    let keys: Vec<_> = url.query_pairs().map(|(key, _)| key.into_owned()).collect();
                    if !keys.is_empty() {
                        url.query_pairs_mut()
                            .clear()
                            .extend_pairs(keys.into_iter().map(|key| (key, "redacted")));
                    }
                    url.set_fragment(None);
                    return token.replace(trimmed, url.as_str());
                }
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub(super) fn bounded_tail(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    if max < '…'.len_utf8() {
        return String::new();
    }
    let keep = max - '…'.len_utf8();
    let mut start = value.len().saturating_sub(keep);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &value[start..])
}

/// Suffix of a line that `bounded_lines` cut at its byte limit (user-visible in error text).
fn truncation_marker() -> &'static str {
    crate::i18n::ui(" [satır kesildi]", " [line truncated]")
}

/// Whether `bounded_lines` cut this line; both languages' markers count, as the interface
/// language can change while a tool keeps running.
fn line_was_truncated(line: &str) -> bool {
    line.ends_with(" [satır kesildi]") || line.ends_with(" [line truncated]")
}

pub(super) fn bounded_lines<R: Read>(
    reader: R,
    limit: usize,
) -> impl Iterator<Item = std::io::Result<String>> {
    let mut reader = BufReader::new(reader);
    std::iter::from_fn(move || {
        let mut line = Vec::new();
        let mut seen = false;
        let mut truncated = false;
        loop {
            let buffer = match reader.fill_buf() {
                Ok(b) => b,
                Err(e) => return Some(Err(e)),
            };
            if buffer.is_empty() {
                return seen.then(|| {
                    Ok(format!(
                        "{}{}",
                        String::from_utf8_lossy(&line),
                        if truncated { truncation_marker() } else { "" }
                    ))
                });
            }
            seen = true;
            let end = buffer
                .iter()
                .position(|b| *b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(buffer.len());
            let newline = buffer[end - 1] == b'\n';
            let keep = end.min(limit.saturating_sub(line.len()));
            truncated |= keep < end;
            line.extend_from_slice(&buffer[..keep]);
            reader.consume(end);
            if newline {
                return Some(Ok(format!(
                    "{}{}",
                    String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']),
                    if truncated { truncation_marker() } else { "" }
                )));
            }
        }
    })
}
