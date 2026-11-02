use super::*;
use std::fs;

static LIVE_JOBS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Stands in for the media analysis, which marks a live source on the job's control.
fn mark_as_live_recording(id: &str) {
    LIVE_JOBS.lock().unwrap().insert(id.to_owned());
}

pub(super) fn pretends_live(id: &str) -> bool {
    LIVE_JOBS.lock().unwrap().contains(id)
}

#[test]
fn daily_window_handles_overnight_boundaries() {
    assert_eq!(parse_minute("23:30"), Some(1410));
    assert_eq!(parse_minute("00:15"), Some(15));
    assert_eq!(parse_minute("24:00"), None);
    assert_eq!(parse_minute("12:60"), None);
}

#[test]
fn schedule_respects_normal_overnight_and_full_day_windows() {
    let mut settings = Settings {
        schedule_enabled: true,
        schedule_start: "09:00".into(),
        schedule_end: "17:00".into(),
        ..Default::default()
    };
    assert!(schedule_open_at(&settings, 9 * 60));
    assert!(schedule_open_at(&settings, 16 * 60 + 59));
    assert!(!schedule_open_at(&settings, 17 * 60));
    assert!(!schedule_open_at(&settings, 8 * 60 + 59));

    settings.schedule_start = "23:00".into();
    settings.schedule_end = "01:00".into();
    assert!(schedule_open_at(&settings, 23 * 60));
    assert!(schedule_open_at(&settings, 30));
    assert!(!schedule_open_at(&settings, 12 * 60));

    settings.schedule_end = "23:00".into();
    assert!(schedule_open_at(&settings, 12 * 60));
}

#[test]
fn overnight_weekday_quota_uses_the_start_day_at_different_utc_offsets() {
    use chrono::FixedOffset;

    let mut queue = QueuePolicy {
        id: "night".into(),
        windows: vec![QueueWindow {
            weekdays: vec![0],
            start: "23:00".into(),
            end: "01:00".into(),
        }],
        quota: Some(QueueQuota {
            limit_bytes: 100,
            ..Default::default()
        }),
        ..Default::default()
    };
    let summer = FixedOffset::west_opt(4 * 3600)
        .unwrap()
        .with_ymd_and_hms(2026, 9, 8, 0, 30, 0)
        .single()
        .unwrap();
    let winter = FixedOffset::west_opt(5 * 3600)
        .unwrap()
        .with_ymd_and_hms(2026, 9, 8, 0, 30, 0)
        .single()
        .unwrap();
    assert_eq!(
        queue_period_key(&queue, &summer),
        queue_period_key(&queue, &winter)
    );
    assert!(queue_period_key(&queue, &summer)
        .unwrap()
        .contains("2026-09-07"));

    let mut settings = Settings {
        queues: vec![queue.clone()],
        ..Default::default()
    };
    consume_queue_bytes_at(&mut settings, "night", 100, &summer);
    assert!(!queue_accepts_work(&settings, "night", &winter));
    let next_occurrence = FixedOffset::west_opt(5 * 3600)
        .unwrap()
        .with_ymd_and_hms(2026, 9, 14, 23, 0, 0)
        .single()
        .unwrap();
    assert!(queue_accepts_work(&settings, "night", &next_occurrence));
    consume_queue_bytes_at(&mut settings, "night", 1, &next_occurrence);
    queue = settings.queues.remove(0);
    assert_eq!(queue.quota.unwrap().consumed_bytes, 1);
}

/// US Eastern time around its 2026 change back to standard time: UTC-4 until
/// 2026-11-01 06:00 UTC, UTC-5 afterwards, so 01:00-02:00 local occurs twice.
#[derive(Clone, Copy, Debug)]
struct EasternFallBack;

impl EasternFallBack {
    fn change() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(6, 0, 0)
            .unwrap()
    }
}

impl TimeZone for EasternFallBack {
    type Offset = chrono::FixedOffset;

    fn from_offset(_: &chrono::FixedOffset) -> Self {
        Self
    }

    fn offset_from_local_date(
        &self,
        local: &chrono::NaiveDate,
    ) -> chrono::LocalResult<chrono::FixedOffset> {
        self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
    }

    fn offset_from_local_datetime(
        &self,
        local: &chrono::NaiveDateTime,
    ) -> chrono::LocalResult<chrono::FixedOffset> {
        let daylight = chrono::FixedOffset::west_opt(4 * 3600).unwrap();
        let standard = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
        let repeated_from = Self::change() - chrono::Duration::hours(5);
        if *local < repeated_from {
            chrono::LocalResult::Single(daylight)
        } else if *local < repeated_from + chrono::Duration::hours(1) {
            chrono::LocalResult::Ambiguous(daylight, standard)
        } else {
            chrono::LocalResult::Single(standard)
        }
    }

    fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> chrono::FixedOffset {
        self.offset_from_utc_datetime(&utc.and_hms_opt(12, 0, 0).unwrap())
    }

    fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> chrono::FixedOffset {
        let hours = if *utc < Self::change() { 4 } else { 5 };
        chrono::FixedOffset::west_opt(hours * 3600).unwrap()
    }
}

#[test]
fn overnight_weekday_quota_period_holds_across_a_real_fall_back_inside_the_window() {
    // Saturday 22:00 to Sunday 04:00 contains the change back to standard time.
    let queue = QueuePolicy {
        id: "night".into(),
        windows: vec![QueueWindow {
            weekdays: vec![5],
            start: "22:00".into(),
            end: "04:00".into(),
        }],
        quota: Some(QueueQuota {
            limit_bytes: 100,
            ..Default::default()
        }),
        ..Default::default()
    };
    let at = |month, day, hour, minute| {
        EasternFallBack.from_utc_datetime(
            &chrono::NaiveDate::from_ymd_opt(2026, month, day)
                .unwrap()
                .and_hms_opt(hour, minute, 0)
                .unwrap(),
        )
    };
    let first_pass = at(11, 1, 5, 30);
    let second_pass = at(11, 1, 6, 30);
    let after = at(11, 1, 8, 30);
    assert_eq!((first_pass.hour(), first_pass.minute()), (1, 30));
    assert_eq!((second_pass.hour(), second_pass.minute()), (1, 30));
    assert_ne!(first_pass.offset(), second_pass.offset());

    let key = queue_period_key(&queue, &first_pass).unwrap();
    assert!(key.contains("2026-10-31"), "{key}");
    assert_eq!(queue_period_key(&queue, &second_pass), Some(key.clone()));
    assert_eq!(queue_period_key(&queue, &after), Some(key));

    let mut settings = Settings {
        queues: vec![queue],
        ..Default::default()
    };
    consume_queue_bytes_at(&mut settings, "night", 100, &first_pass);
    assert!(!queue_accepts_work(&settings, "night", &second_pass));
    assert!(!queue_accepts_work(&settings, "night", &after));
    assert!(queue_accepts_work(&settings, "night", &at(11, 8, 3, 30)));
}

#[test]
fn refresh_keeps_player_profile_and_exact_track_selection() {
    let mut previous = AddRequest {
        url: "https://media.example.test/video.m3u8?token=old".into(),
        kind: DownloadKind::Video,
        page_url: Some("https://example.test/watch/7#player".into()),
        source_identity: Some(SourceIdentity {
            video_id: "player-7".into(),
            frame_id: 12,
            document_id: Some("old-document".into()),
            page_url: "https://example.test/watch/7".into(),
        }),
        audio_tracks: vec![AudioSelection {
            id: "audio-tr".into(),
            language: Some("tr".into()),
        }],
        subtitle_tracks: vec![SubtitleSelection {
            language: "tr".into(),
            automatic: false,
        }],
        expected_audio: Some(true),
        ..Default::default()
    };
    previous.session_cookies.push(ScopedCookie {
        store_id: Some("profile-1".into()),
        ..Default::default()
    });
    let mut refreshed = previous.clone();
    refreshed.url = "https://media.example.test/video.m3u8?token=new".into();
    refreshed.source_identity.as_mut().unwrap().document_id = Some("new-document".into());
    assert!(refresh_identity_matches(&previous, &refreshed));

    refreshed.audio_tracks[0].id = "audio-en".into();
    assert!(!refresh_identity_matches(&previous, &refreshed));
    refreshed.audio_tracks = previous.audio_tracks.clone();
    refreshed.source_identity.as_mut().unwrap().frame_id = 13;
    assert!(!refresh_identity_matches(&previous, &refreshed));
    refreshed.source_identity.as_mut().unwrap().frame_id = 12;
    refreshed.session_cookies[0].store_id = Some("profile-2".into());
    assert!(!refresh_identity_matches(&previous, &refreshed));

    let mut without_cookies = previous.clone();
    without_cookies.session_cookies.clear();
    let mut unscoped = without_cookies.clone();
    unscoped.session_cookies.push(ScopedCookie {
        name: "account".into(),
        value: "private-a".into(),
        ..Default::default()
    });
    assert!(!refresh_identity_matches(&without_cookies, &unscoped));
    let mut other_unscoped = unscoped.clone();
    other_unscoped.session_cookies[0].value = "private-b".into();
    assert!(!refresh_identity_matches(&unscoped, &other_unscoped));
    assert!(refresh_identity_matches(&without_cookies, &without_cookies));
}

fn refresh_request(url: String) -> AddRequest {
    AddRequest {
        url,
        kind: DownloadKind::File,
        filename: Some("refresh.bin".into()),
        page_url: Some("https://example.test/watch/refresh".into()),
        source_identity: Some(SourceIdentity {
            video_id: "refresh-video".into(),
            frame_id: 7,
            document_id: None,
            page_url: "https://example.test/watch/refresh".into(),
        }),
        start_at: Some(Utc::now().timestamp() + 3600),
        ..Default::default()
    }
}

fn seed_direct_refresh_state(engine: &Engine, id: &str) -> (PathBuf, Vec<u8>) {
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    let output = refresh_output_path(job).unwrap();
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(append_path_suffix(&output, ".ssdownload.part"), b"x").unwrap();
    let state_path = append_path_suffix(&output, ".ssdownload.state");
    let state = serde_json::json!({
        "version": 3,
        "url": "old-source",
        "etag": "\"stable-v1\"",
        "last_modified": null,
        "total": 4,
        "segmented": false,
        "connections": 1,
        "checksum": null,
        "completed": [{"start": 0, "end": 1}]
    });
    let bytes = serde_json::to_vec(&state).unwrap();
    fs::write(&state_path, &bytes).unwrap();
    (state_path, bytes)
}

fn read_head_and_respond(
    listener: std::net::TcpListener,
    started: Option<mpsc::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
) {
    use std::io::{Read as _, Write as _};
    let (mut stream, _) = listener.accept().unwrap();
    let mut request = Vec::new();
    let mut buffer = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
    }
    assert!(request.starts_with(b"HEAD "));
    if let Some(started) = started {
        started.send(()).unwrap();
    }
    if let Some(release) = release {
        release.recv().unwrap();
    }
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"stable-v1\"\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

#[test]
fn slow_source_refresh_keeps_actor_responsive_and_cancel_discards_preparation() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-refresh-cancel-{}",
        Uuid::new_v4()
    ));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(refresh_request(
            "https://old.example.test/refresh.bin".into(),
        ))
        .unwrap()
        .remove(0);
    let (state_path, original_state) = seed_direct_refresh_state(&engine, &id);
    let ticket = engine.begin_source_refresh(&id).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let refreshed_url = format!("http://{}/refresh.bin", listener.local_addr().unwrap());
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server =
        thread::spawn(move || read_head_and_respond(listener, Some(started_tx), Some(release_rx)));
    let complete_engine = engine.clone();
    let complete_id = id.clone();
    let complete = thread::spawn(move || {
        let mut request = (*ticket.request).clone();
        request.url = refreshed_url;
        complete_engine.complete_source_refresh(&complete_id, &ticket.token, request, false)
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let status_started = Instant::now();
    assert_eq!(
        engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .state,
        JobState::AwaitingSource
    );
    assert!(status_started.elapsed() < Duration::from_secs(1));
    let pause_started = Instant::now();
    engine.pause(&id).unwrap();
    assert!(pause_started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .state,
        JobState::Paused
    );
    release_tx.send(()).unwrap();
    assert!(complete.join().unwrap().is_err());
    server.join().unwrap();
    assert_eq!(fs::read(state_path).unwrap(), original_state);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn superseded_source_refresh_completion_cannot_commit() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-refresh-stale-{}",
        Uuid::new_v4()
    ));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(refresh_request(
            "https://old.example.test/refresh.bin".into(),
        ))
        .unwrap()
        .remove(0);
    let (state_path, original_state) = seed_direct_refresh_state(&engine, &id);
    let old_ticket = engine.begin_source_refresh(&id).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let refreshed_url = format!("http://{}/refresh.bin", listener.local_addr().unwrap());
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server =
        thread::spawn(move || read_head_and_respond(listener, Some(started_tx), Some(release_rx)));
    let complete_engine = engine.clone();
    let complete_id = id.clone();
    let old_token = old_ticket.token.clone();
    let worker_token = old_token.clone();
    let complete = thread::spawn(move || {
        let mut request = (*old_ticket.request).clone();
        request.url = refreshed_url;
        complete_engine.complete_source_refresh(&complete_id, &worker_token, request, false)
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let new_ticket = engine.begin_source_refresh(&id).unwrap();
    assert_ne!(new_ticket.token, old_token);
    release_tx.send(()).unwrap();
    assert!(complete.join().unwrap().is_err());
    server.join().unwrap();
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.jobs.iter().find(|job| job.id == id).unwrap().state,
        JobState::AwaitingSource
    );
    assert_eq!(fs::read(state_path).unwrap(), original_state);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_refresh_save_failure_rolls_back_staged_direct_state() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-refresh-rollback-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let id = engine
        .add(refresh_request(
            "https://old.example.test/refresh.bin".into(),
        ))
        .unwrap()
        .remove(0);
    let (state_path, original_state) = seed_direct_refresh_state(&engine, &id);
    let ticket = engine.begin_source_refresh(&id).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut request = (*ticket.request).clone();
    request.url = format!("http://{}/refresh.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || read_head_and_respond(listener, None, None));
    rusqlite::Connection::open(&paths.database)
        .unwrap()
        .execute("DROP TABLE jobs", [])
        .unwrap();
    let result = engine.complete_source_refresh(&id, &ticket.token, request, false);
    server.join().unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read(state_path).unwrap(), original_state);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_failed_file_job_is_logged_at_the_file_stage_and_not_at_a_media_stage() {
    let job_of = |request: AddRequest| Job {
        id: "job".into(),
        request,
        name: "report.pdf".into(),
        state: JobState::Failed,
        path: PathBuf::from("report.pdf"),
        downloaded: 0,
        total: None,
        speed: 0,
        eta: None,
        error: None,
        phase: JobState::Failed.label().into(),
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
        live: false,
        sequence: 0,
    };
    let request = |kind| AddRequest {
        url: "https://files.example.test/a".into(),
        kind,
        ..Default::default()
    };
    let detail = |request: AddRequest| {
        let message = crate::error_codes::coded_default(crate::error_codes::NET_004).to_string();
        let event = failure_event(&job_of(request), &message);
        assert_eq!(event.code, Some("SSD-NET-004"));
        event.detail.unwrap()
    };
    assert!(detail(request(DownloadKind::File)).starts_with("aşama=dosya "));
    assert!(detail(request(DownloadKind::Video)).starts_with("aşama=medya kaynağı "));
    // A file job that asks for a media selection runs in the media pipeline.
    let selected = AddRequest {
        format_id: Some("18".into()),
        ..request(DownloadKind::File)
    };
    assert!(detail(selected).starts_with("aşama=medya kaynağı "));
}

#[test]
fn completion_requires_every_remaining_job_to_succeed() {
    let make_job = |id: &str, state| Job {
        id: id.into(),
        request: AddRequest {
            queue_id: Some("queue".into()),
            ..Default::default()
        },
        name: id.into(),
        state,
        path: PathBuf::from(id),
        downloaded: 0,
        total: None,
        speed: 0,
        eta: None,
        error: None,
        phase: state.label().into(),
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
        live: false,
        sequence: 0,
    };
    let mut jobs = BTreeMap::new();
    jobs.insert("done".into(), make_job("done", JobState::Completed));
    jobs.insert("failed".into(), make_job("failed", JobState::Failed));
    assert!(!queue_completed_successfully(&jobs, "queue"));
    jobs.insert("failed".into(), make_job("failed", JobState::Completed));
    assert!(queue_completed_successfully(&jobs, "queue"));
    jobs.clear();
    assert!(!queue_completed_successfully(&jobs, "queue"));
}

#[test]
fn folder_rule_subdomains_require_a_label_boundary() {
    let rule = FolderRule {
        id: "example".into(),
        host: "example.com".into(),
        include_subdomains: true,
        destination: PathBuf::from(r"C:\\Downloads\\Example"),
        priority: 1,
        enabled: true,
        ..Default::default()
    };
    assert!(matching_folder(
        std::slice::from_ref(&rule),
        &Url::parse("https://cdn.example.com/file").unwrap(),
        DownloadKind::File
    )
    .is_some());
    assert!(matching_folder(
        &[rule],
        &Url::parse("https://fakeexample.com/file").unwrap(),
        DownloadKind::File
    )
    .is_none());
}

/// An opt-in measured scenario for the actor/snapshot path. All jobs are
/// scheduled in the future, so it makes no network request and uses only a
/// fresh temporary application directory.
#[test]
#[ignore = "measured queue benchmark; run explicitly"]
fn measured_large_scheduled_queue_and_snapshot_reads() {
    const JOBS: usize = 500;
    const SNAPSHOTS: usize = 2_000;
    let root = std::env::temp_dir().join(format!("ssdownload-engine-bench-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    let output = root.join("output");
    fs::create_dir_all(&output).unwrap();
    let engine = Engine::open(paths).unwrap();
    let start = Instant::now();
    let future = Utc::now().timestamp() + 3600;
    for index in 0..JOBS {
        engine
            .add(AddRequest {
                url: format!("https://benchmark.example.test/files/{index}"),
                filename: Some(format!("file-{index}.bin")),
                directory: Some(output.clone()),
                request_id: Some(format!("benchmark-{index}")),
                start_at: Some(future),
                ..Default::default()
            })
            .unwrap();
    }
    let queued = engine.snapshot();
    assert_eq!(queued.jobs.len(), JOBS);
    for _ in 0..SNAPSHOTS {
        assert_eq!(engine.snapshot().jobs.len(), JOBS);
    }
    let elapsed = start.elapsed();
    println!(
        "MEASURED_ENGINE_QUEUE jobs={JOBS} snapshots={SNAPSHOTS} elapsed_ms={}",
        elapsed.as_millis()
    );
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Measures only the engine actor while no jobs are queued. It deliberately
/// excludes GUI paint, clipboard, browser bridge and download work.
#[test]
#[ignore = "measured headless engine idle CPU; run explicitly"]
fn measured_headless_engine_idle_cpu() {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };

    fn process_cpu_100ns() -> u64 {
        let zero = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut created = zero();
        let mut exited = zero();
        let mut kernel = zero();
        let mut user = zero();
        assert_ne!(
            unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exited,
                    &mut kernel,
                    &mut user,
                )
            },
            0
        );
        let as_u64 = |value: FILETIME| {
            u64::from(value.dwLowDateTime) | (u64::from(value.dwHighDateTime) << 32)
        };
        as_u64(kernel) + as_u64(user)
    }

    let root = std::env::temp_dir().join(format!("ssdownload-engine-idle-{}", Uuid::new_v4()));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let before = process_cpu_100ns();
    let wall = Instant::now();
    thread::sleep(Duration::from_secs(10));
    let elapsed = wall.elapsed();
    let cpu_ms = (process_cpu_100ns() - before) / 10_000;
    println!(
        "MEASURED_ENGINE_IDLE wall_ms={} process_cpu_ms={cpu_ms}",
        elapsed.as_millis()
    );
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn browser_policy_stop_persists_scheduled_state_before_releasing_generation() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-policy-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    settings.queues[0].quota = Some(QueueQuota {
        limit_bytes: 1,
        ..Default::default()
    });
    engine.update_settings(settings).unwrap();
    let id = engine
        .add(AddRequest {
            url: "https://example.test/browser.bin".into(),
            filename: Some("browser.bin".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    engine.shutdown();

    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    job.browser_transfer_authorized = true;
    store.save_job(&job).unwrap();
    drop(store);

    let engine = Engine::open(paths.clone()).unwrap();
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let (_, generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, false)
        .unwrap();
    assert!(engine
        .browser_transfer_progress(&id, generation, 1, Some(2))
        .is_err());
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(job.state, JobState::Scheduled);
    assert!(job.phase.contains("sınırında durdu"));
    // The stop is already in the database while the engine is still running.
    let stored = Store::open(&paths.database)
        .unwrap()
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    assert_eq!(stored.state, JobState::Scheduled);
    assert!(stored.phase.contains("sınırında durdu"));
    assert!(engine.browser_transfer_job(&id).is_err());
    assert!(engine.pause_browser_transfer(&id, generation).is_err());
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn browser_transfer_refusals_carry_their_code() {
    use crate::error_codes::{code_of_text, TRF_014};
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-refusal-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths).unwrap();
    let id = engine
        .add(AddRequest {
            url: "https://example.test/browser-refused.bin".into(),
            filename: Some("browser-refused.bin".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let refusal = |id: &str| {
        engine
            .begin_browser_transfer(id, std::process::id(), sender_started_at, false)
            .map(|_| ())
            .unwrap_err()
    };
    assert_eq!(
        code_of_text(&refusal(&id)),
        Some(TRF_014),
        "browser transfer is off"
    );
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    assert_eq!(
        code_of_text(&refusal("00000000-0000-4000-8000-000000000000")),
        Some(TRF_014),
        "an unknown job"
    );
    assert_eq!(
        code_of_text(&refusal(&id)),
        Some(TRF_014),
        "a job not authorized for a browser transfer"
    );
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn disconnected_sender_releases_only_its_generation() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-sender-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let id = engine
        .add(AddRequest {
            url: "https://example.test/browser-sender.bin".into(),
            filename: Some("browser-sender.bin".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    engine.shutdown();

    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    job.browser_transfer_authorized = true;
    store.save_job(&job).unwrap();
    drop(store);

    let engine = Engine::open(paths).unwrap();
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let (_, first_generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, false)
        .unwrap();
    engine
        .release_browser_transfer(&id, first_generation)
        .unwrap();
    let paused = engine.snapshot();
    let job = paused.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(job.state, JobState::Paused);
    assert!(job.phase.contains("bağlantısı koptu"));

    let (_, current_generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, false)
        .unwrap();
    assert_ne!(first_generation, current_generation);
    engine
        .release_browser_transfer(&id, first_generation)
        .unwrap();
    assert_eq!(
        engine.browser_transfer_job(&id).unwrap().1,
        current_generation
    );
    engine
        .release_browser_transfer(&id, current_generation)
        .unwrap();
    assert_eq!(
        engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .state,
        JobState::Paused
    );
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_restarted_browser_transfer_counts_its_bytes_again_and_a_resumed_one_does_not() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-quota-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(AddRequest {
            url: "https://example.test/browser-quota.bin".into(),
            filename: Some("browser-quota.bin".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    engine.shutdown();

    // A browser transfer of the job had committed 1 MB when it was paused.
    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    job.browser_transfer_authorized = true;
    job.downloaded = 1_000_000;
    store.save_job(&job).unwrap();
    drop(store);

    let engine = Engine::open(paths).unwrap();
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let total = Some(4_000_000);
    // Resumed, the transfer reports the bytes it committed before and those it adds.
    let (_, generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, false)
        .unwrap();
    engine
        .browser_transfer_prepared(&id, generation, 1_000_000)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 1_300_000, total)
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);
    engine.release_browser_transfer(&id, generation).unwrap();

    // Restarted, it downloads the file again from the first byte.
    let (_, generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, true)
        .unwrap();
    engine
        .browser_transfer_prepared(&id, generation, 0)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 200_000, total)
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 500_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Adds a job of `url`, sets it up in the store as `set_up` says while the engine is closed, with
/// browser transfers on and a quota on the default queue.
fn browser_quota_job(
    name: &str,
    url: String,
    set_up: impl FnOnce(&mut Job),
) -> (PathBuf, AppPaths, String) {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-{name}-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(AddRequest {
            filename: Some(format!("{name}.bin")),
            page_url: Some(format!("https://example.test/watch/{name}")),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..file_request(url, &root.join("out"))
        })
        .unwrap()
        .remove(0);
    engine.shutdown();
    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    set_up(&mut job);
    store.save_job(&job).unwrap();
    (root, paths, id)
}

fn begin_as_this_process(engine: &Engine, id: &str, restart: bool) -> u64 {
    let started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    engine
        .begin_browser_transfer(id, std::process::id(), started_at, restart)
        .unwrap()
        .1
}

#[test]
fn a_transfer_goes_on_from_the_count_after_a_begin_that_ended_early() {
    // A browser transfer of the job had committed and counted 1 MB.
    let (root, paths, id) = browser_quota_job(
        "browser-early",
        "https://example.test/browser-early.bin".into(),
        |job| {
            job.browser_transfer_authorized = true;
            job.downloaded = 1_000_000;
        },
    );
    let engine = Engine::open(paths.clone()).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    // The app closes before the receiver is ready, and the receiver of the next session has lost
    // the bytes: they come again and are not counted again.
    begin_as_this_process(&engine, &id, false);
    engine.shutdown();
    let engine = Engine::open(paths).unwrap();
    let generation = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, generation, 0)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 1_200_000, Some(4_000_000))
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 200_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_sent_to_the_browser_counts_again_after_a_begin_that_ended_early() {
    // The app's own downloader had counted 1 MB.
    let (root, paths, id) = browser_quota_job(
        "browser-handover",
        "https://example.test/browser-handover.bin".into(),
        |job| job.downloaded = 1_000_000,
    );
    let engine = Engine::open(paths).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    engine.begin_source_refresh(&id).unwrap();
    // The receiver fails before it is ready, which pauses the transfer; a state it saved by
    // then holds no byte.
    let generation = begin_as_this_process(&engine, &id, false);
    engine.pause_browser_transfer(&id, generation).unwrap();

    let generation = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, generation, 0)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 300_000, Some(4_000_000))
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_restart_counts_again_after_a_begin_that_ended_early() {
    // A browser transfer of the job had committed and counted 1 MB.
    let (root, paths, id) = browser_quota_job(
        "browser-restart",
        "https://example.test/browser-restart.bin".into(),
        |job| {
            job.browser_transfer_authorized = true;
            job.downloaded = 1_000_000;
        },
    );
    let engine = Engine::open(paths.clone()).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    let total = Some(4_000_000);
    // A restart whose receiver fails before it is ready, and the begin after it, whose receiver
    // goes on from `committed` bytes and reports `downloaded`.
    let restart_then_begin = |engine: &Engine, committed: u64, downloaded: u64| {
        let generation = begin_as_this_process(engine, &id, true);
        engine.pause_browser_transfer(&id, generation).unwrap();
        let generation = begin_as_this_process(engine, &id, false);
        engine
            .browser_transfer_prepared(&id, generation, committed)
            .unwrap();
        engine
            .browser_transfer_progress(&id, generation, downloaded, total)
            .unwrap();
        engine.release_browser_transfer(&id, generation).unwrap();
    };

    // The receiver fails before it moves the committed bytes aside, and the next begin goes on
    // from them.
    restart_then_begin(&engine, 1_000_000, 1_100_000);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 100_000);

    // It fails once it has moved them aside, and the next begin starts from the first byte.
    restart_then_begin(&engine, 0, 200_000);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);

    // The app closes once the receiver has moved them aside, and the begin in the next session
    // starts from the first byte.
    begin_as_this_process(&engine, &id, true);
    engine.shutdown();
    let engine = Engine::open(paths).unwrap();
    let generation = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, generation, 0)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 400_000, total)
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 700_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_report_of_an_earlier_browser_transfer_is_refused() {
    let (root, paths, id) = browser_quota_job(
        "browser-late",
        "https://example.test/browser-late.bin".into(),
        |job| {
            job.browser_transfer_authorized = true;
            job.downloaded = 1_000_000;
        },
    );
    let engine = Engine::open(paths).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    let total = Some(4_000_000);
    let earlier = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, earlier, 1_000_000)
        .unwrap();
    engine.release_browser_transfer(&id, earlier).unwrap();

    // A chunk of the earlier transfer that the receiver still held reports after the restart
    // began; the restart's own first report follows it.
    let restarted = begin_as_this_process(&engine, &id, true);
    assert!(engine
        .browser_transfer_progress(&id, earlier, 1_100_000, total)
        .is_err());
    engine.browser_transfer_prepared(&id, restarted, 0).unwrap();
    engine
        .browser_transfer_progress(&id, restarted, 200_000, total)
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 200_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_sent_to_the_browser_stays_as_it_was_when_the_app_closes_before_the_receiver_is_ready() {
    // The job waits for a new source, which the browser's session gives; the app's own
    // downloader must not run it.
    let server = FileServer::start(vec![
        ("/browser-reopen.bin".into(), vec![6; 4096], false),
        ("/other.bin".into(), vec![7; 4096], false),
    ]);
    let (root, paths, id) =
        browser_quota_job("browser-reopen", server.url("/browser-reopen.bin"), |job| {
            job.downloaded = 1_000_000
        });
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    begin_as_this_process(&engine, &id, false);
    engine.shutdown();

    let engine = Engine::open(paths).unwrap();
    // A job that the start queued would have been asked for by the time another one completes.
    let other = engine
        .add(file_request(server.url("/other.bin"), &root.join("out")))
        .unwrap()
        .remove(0);
    wait_until("the other download", || {
        job_state(&engine, &other) == JobState::Completed
    });
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(
        (job.state, job.browser_transfer_authorized, job.downloaded),
        (JobState::AwaitingSource, false, 1_000_000)
    );
    assert!(!server
        .hits
        .lock()
        .unwrap()
        .iter()
        .any(|hit| hit == "/browser-reopen.bin"));
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Opens the app again and lets another download of `server` complete, by when the start would
/// have asked for the job had it queued it. Returns the job and whether its file at `path` was
/// asked for.
fn after_a_restart(
    paths: AppPaths,
    root: &Path,
    server: &FileServer,
    id: &str,
    path: &str,
) -> (Job, bool) {
    let engine = Engine::open(paths).unwrap();
    let other = engine
        .add(file_request(server.url("/other.bin"), &root.join("out")))
        .unwrap()
        .remove(0);
    wait_until("the other download", || {
        job_state(&engine, &other) == JobState::Completed
    });
    let job = engine
        .snapshot()
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap()
        .clone();
    let asked = server.hits.lock().unwrap().iter().any(|hit| hit == path);
    engine.shutdown();
    (job, asked)
}

/// Sends a job that waits for a new source after an error to the browser, lets `change` change the
/// job before the receiver is ready and closes the app. Returns the job as it was saved before the
/// begin, the job once the app is open again (see `after_a_restart`) and whether its file was
/// asked for.
fn changed_before_the_receiver_is_ready(
    name: &str,
    change: impl FnOnce(&Engine, &str),
) -> (Job, Job, bool) {
    let path = format!("/{name}.bin");
    let server = FileServer::start(vec![
        (path.clone(), vec![6; 4096], false),
        ("/other.bin".into(), vec![7; 4096], false),
    ]);
    let (root, paths, id) = browser_quota_job(name, server.url(&path), |job| {
        job.downloaded = 1_000_000;
        job.error = Some("The signed address expired.".into());
    });
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    let before = Store::open(&paths.database)
        .unwrap()
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    begin_as_this_process(&engine, &id, false);
    change(&engine, &id);
    engine.shutdown();

    let (after, asked) = after_a_restart(paths, &root, &server, &id, &path);
    fs::remove_dir_all(root).unwrap();
    (before, after, asked)
}

#[test]
fn a_job_sent_to_the_browser_and_moved_up_stays_as_it_was_when_the_app_closes_before_the_receiver_is_ready(
) {
    // The user moves the job to the top of the list before the receiver is ready.
    let (before, after, asked) =
        changed_before_the_receiver_is_ready("browser-moved", |engine, id| {
            engine.reorder(vec![id.into()], true).unwrap()
        });
    assert_eq!(
        (
            after.state,
            after.phase,
            after.error,
            after.browser_transfer_authorized,
            after.downloaded,
            after.priority > before.priority,
            asked
        ),
        (
            JobState::AwaitingSource,
            before.phase,
            before.error,
            false,
            1_000_000,
            true,
            false
        )
    );
}

#[test]
fn a_job_sent_to_the_browser_and_set_to_open_when_done_stays_as_it_was_when_the_app_closes_before_the_receiver_is_ready(
) {
    let (before, after, asked) =
        changed_before_the_receiver_is_ready("browser-open", |engine, id| {
            engine.set_open_when_done(id, true).unwrap()
        });
    assert_eq!(
        (
            after.state,
            after.phase,
            after.error,
            after.browser_transfer_authorized,
            after.downloaded,
            (before.open_when_done, after.open_when_done),
            asked
        ),
        (
            JobState::AwaitingSource,
            before.phase,
            before.error,
            false,
            1_000_000,
            (false, true),
            false
        )
    );
}

#[test]
fn a_job_sent_to_the_browser_is_not_started_now_before_the_receiver_is_ready() {
    // A download of the job's file would stay running until the end.
    let server = FileServer::start(vec![
        ("/browser-start-now.bin".into(), vec![6; 4096], true),
        ("/other.bin".into(), vec![7; 4096], false),
    ]);
    let (root, paths, id) = browser_quota_job(
        "browser-start-now",
        server.url("/browser-start-now.bin"),
        |job| job.downloaded = 1_000_000,
    );
    let engine = Engine::open(paths).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    let generation = begin_as_this_process(&engine, &id, false);
    let started = engine.start_now(&id);
    // A job that the app's own downloader ran would have been asked for by the time another
    // download completes.
    let other = engine
        .add(file_request(server.url("/other.bin"), &root.join("out")))
        .unwrap()
        .remove(0);
    wait_until("the other download", || {
        job_state(&engine, &other) == JobState::Completed
    });
    let asked = server
        .hits
        .lock()
        .unwrap()
        .iter()
        .any(|hit| hit == "/browser-start-now.bin");
    // The transfer goes on: its receiver gets ready.
    let prepared = engine.browser_transfer_prepared(&id, generation, 0);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        (started.is_err(), asked, prepared.is_ok()),
        (true, false, true)
    );
}

#[test]
fn a_job_sent_to_the_browser_is_not_resumed_before_the_receiver_is_ready() {
    let server = FileServer::start(vec![
        ("/browser-resume.bin".into(), vec![6; 4096], false),
        ("/other.bin".into(), vec![7; 4096], false),
    ]);
    let (root, paths, id) =
        browser_quota_job("browser-resume", server.url("/browser-resume.bin"), |job| {
            job.downloaded = 1_000_000
        });
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    begin_as_this_process(&engine, &id, false);
    let resumed = engine.resume(&id);
    engine.shutdown();

    let (job, asked) = after_a_restart(paths, &root, &server, &id, "/browser-resume.bin");
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        (resumed.is_err(), job.state, asked),
        (true, JobState::AwaitingSource, false)
    );
}

#[test]
fn a_job_sent_to_the_browser_stays_as_it_was_when_the_jobs_are_saved_again_after_a_store_error() {
    let server = FileServer::start(vec![
        ("/browser-store-error.bin".into(), vec![6; 4096], false),
        ("/scheduled.bin".into(), vec![8; 4096], false),
        ("/other.bin".into(), vec![7; 4096], false),
    ]);
    let (root, paths, id) = browser_quota_job(
        "browser-store-error",
        server.url("/browser-store-error.bin"),
        |job| job.downloaded = 1_000_000,
    );
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    begin_as_this_process(&engine, &id, false);
    // Before the receiver is ready, another job's start time comes while no job can be written.
    // The engine stops and, once it can write again, saves every job it has.
    engine
        .add(AddRequest {
            start_at: Some(Utc::now().timestamp() + 3),
            ..file_request(server.url("/scheduled.bin"), &root.join("out"))
        })
        .unwrap();
    let database = rusqlite::Connection::open(&paths.database).unwrap();
    database
        .execute("ALTER TABLE jobs RENAME TO jobs_aside", [])
        .unwrap();
    let store_error = || {
        engine
            .snapshot()
            .warning
            .is_some_and(|warning| warning.contains("no such table: jobs"))
    };
    wait_until("the store error", &store_error);
    database
        .execute("ALTER TABLE jobs_aside RENAME TO jobs", [])
        .unwrap();
    drop(database);
    wait_until("the jobs to be saved again", || !store_error());
    engine.shutdown();

    let (job, asked) = after_a_restart(paths, &root, &server, &id, "/browser-store-error.bin");
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        (
            job.state,
            job.browser_transfer_authorized,
            job.downloaded,
            asked
        ),
        (JobState::AwaitingSource, false, 1_000_000, false)
    );
}

#[test]
fn a_job_sent_to_the_browser_is_saved_as_it_is_once_the_user_pauses_it_before_the_receiver_is_ready(
) {
    let server = FileServer::start(vec![("/browser-paused.bin".into(), vec![6; 4096], true)]);
    let (root, paths, id) =
        browser_quota_job("browser-paused", server.url("/browser-paused.bin"), |_| {});
    let engine = Engine::open(paths.clone()).unwrap();
    let saved = || {
        Store::open(&paths.database)
            .unwrap()
            .load_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.id == id)
            .unwrap()
    };
    engine.begin_source_refresh(&id).unwrap();
    begin_as_this_process(&engine, &id, false);
    engine.pause(&id).unwrap();
    let paused = saved().state;

    // Resumed in the app, the job runs in the app's own downloader, which saves its progress at
    // most once a second.
    engine.resume(&id).unwrap();
    wait_until("the run", || !server.hits.lock().unwrap().is_empty());
    wait_until("the saved progress", || {
        report_downloaded(&engine, &id, &[1_000]);
        saved().downloaded == 1_000
    });
    let running = saved().state;
    server.release();
    wait_until("the download", || {
        job_state(&engine, &id) == JobState::Completed
    });
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
    assert_eq!((paused, running), (JobState::Paused, JobState::Downloading));
}

#[test]
fn a_restart_sent_to_the_browser_that_ended_early_leaves_the_count_of_the_apps_own_download() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-engine-browser-back-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/browser-back.bin".into(), vec![6; 4096], true)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(AddRequest {
            page_url: Some("https://example.test/watch/browser-back".into()),
            ..file_request(server.url("/browser-back.bin"), &root.join("out"))
        })
        .unwrap()
        .remove(0);
    // The app's own downloader counts 1 MB, which its partial file keeps.
    let runs = || server.hits.lock().unwrap().len();
    wait_until("the first run", || runs() == 1);
    report_downloaded(&engine, &id, &[1_000_000]);
    engine.pause(&id).unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 1_000_000);

    // Sent to the browser with a restart, the transfer ends before its receiver is ready.
    wait_until("the run to stop", || {
        engine.begin_source_refresh(&id).is_ok()
    });
    let generation = begin_as_this_process(&engine, &id, true);
    engine.pause_browser_transfer(&id, generation).unwrap();

    // Resumed in the app, the downloader goes on from its partial file and reports from it.
    engine.resume(&id).unwrap();
    wait_until("the second run", || runs() == 2);
    report_downloaded(&engine, &id, &[1_000_000, 1_200_000]);
    server.release();
    wait_until("the download", || {
        job_state(&engine, &id) == JobState::Completed
    });
    assert_eq!(consumed_quota_bytes(&engine, &queue), 1_200_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_sent_to_the_browser_is_saved_as_authorized_once_the_receiver_is_ready() {
    let (root, paths, id) = browser_quota_job(
        "browser-saved",
        "https://example.test/browser-saved.bin".into(),
        |job| job.downloaded = 1_000_000,
    );
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    let generation = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, generation, 0)
        .unwrap();
    engine.shutdown();

    let engine = Engine::open(paths).unwrap();
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(
        (job.state, job.browser_transfer_authorized, job.downloaded),
        (JobState::Paused, true, 0)
    );
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_sent_to_the_browser_counts_again_the_bytes_its_receiver_does_not_hold() {
    // The app's own downloader had counted 1 MB, and the receiver holds 300 KB of an earlier
    // transfer of the job.
    let (root, paths, id) = browser_quota_job(
        "browser-beyond",
        "https://example.test/browser-beyond.bin".into(),
        |job| job.downloaded = 1_000_000,
    );
    let engine = Engine::open(paths).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    engine.begin_source_refresh(&id).unwrap();
    let generation = begin_as_this_process(&engine, &id, false);
    engine
        .browser_transfer_prepared(&id, generation, 300_000)
        .unwrap();
    engine
        .browser_transfer_progress(&id, generation, 1_300_000, Some(4_000_000))
        .unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 1_000_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_chunk_that_carries_the_token_of_a_transfer_that_ended_is_refused() {
    use crate::browser_transfer::{dispatch, Command as Transfer};
    use base64::Engine as _;
    use sha2::Digest as _;

    let (root, paths, id) = browser_quota_job(
        "browser-old-token",
        "https://example.test/browser-old-token.bin".into(),
        |job| job.browser_transfer_authorized = true,
    );
    let engine = Engine::open(paths.clone()).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    let started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let begun = dispatch(
        &paths,
        &engine,
        Transfer::Begin {
            id: id.clone(),
            total: Some(4_000_000),
            etag: Some("\"browser-old-token\"".into()),
            restart: false,
            sender_pid: Some(std::process::id()),
            sender_started_at: Some(started_at),
        },
    )
    .unwrap();
    let token = begun.token.clone().unwrap();
    let chunk = |offset: u64, sequence: u64| {
        let bytes = vec![7u8; 300_000];
        dispatch(
            &paths,
            &engine,
            Transfer::Chunk {
                id: id.clone(),
                token: token.clone(),
                stream_id: 0,
                sequence,
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                sha256: hex::encode(sha2::Sha256::digest(&bytes)),
                client_checkpoint: None,
            },
        )
    };
    let stream = chunk(0, begun.streams[0].next_sequence).unwrap().streams[0].clone();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);

    // Paused in the app, the transfer ends and the receiver keeps its token. A restart begins,
    // and a chunk of the transfer that ended reaches the receiver before the restart does.
    engine.pause(&id).unwrap();
    let restarted = begin_as_this_process(&engine, &id, true);
    assert!(chunk(stream.offset, stream.next_sequence).is_err());
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);
    assert_eq!(engine.browser_transfer_job(&id).unwrap().1, restarted);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_chunk_that_carries_the_token_of_a_transfer_of_an_earlier_session_is_refused() {
    use crate::browser_transfer::{dispatch, Command as Transfer};
    use base64::Engine as _;
    use sha2::Digest as _;

    let (root, paths, id) = browser_quota_job(
        "browser-session",
        "https://example.test/browser-session.bin".into(),
        |job| job.browser_transfer_authorized = true,
    );
    let engine = Engine::open(paths.clone()).unwrap();
    let queue = engine.snapshot().settings.queues[0].id.clone();
    let started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let begun = dispatch(
        &paths,
        &engine,
        Transfer::Begin {
            id: id.clone(),
            total: Some(4_000_000),
            etag: Some("\"browser-session\"".into()),
            restart: false,
            sender_pid: Some(std::process::id()),
            sender_started_at: Some(started_at),
        },
    )
    .unwrap();
    let token = begun.token.clone().unwrap();
    let chunk = |engine: &Engine, offset: u64, sequence: u64| {
        let bytes = vec![7u8; 300_000];
        dispatch(
            &paths,
            engine,
            Transfer::Chunk {
                id: id.clone(),
                token: token.clone(),
                stream_id: 0,
                sequence,
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                sha256: hex::encode(sha2::Sha256::digest(&bytes)),
                client_checkpoint: None,
            },
        )
    };
    let stream = chunk(&engine, 0, begun.streams[0].next_sequence)
        .unwrap()
        .streams[0]
        .clone();
    engine.shutdown();

    // In the next session a restart begins, and a chunk of the earlier session's transfer reaches
    // the receiver before the restart does.
    let engine = Engine::open(paths.clone()).unwrap();
    let restarted = begin_as_this_process(&engine, &id, true);
    let late = chunk(&engine, stream.offset, stream.next_sequence);
    let consumed = consumed_quota_bytes(&engine, &queue);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
    // The extension takes a generation only as a safe integer.
    let safe = |generation: u64| (1..1 << 53).contains(&generation);
    assert_eq!(
        (
            late.is_err(),
            consumed,
            safe(begun.sender_generation.unwrap()),
            safe(restarted)
        ),
        (true, 300_000, true, true)
    );
}

#[test]
fn a_browser_transfer_counts_its_bytes_once_until_the_user_starts_it_over() {
    use crate::browser_transfer::{dispatch, Command as Transfer, Response};
    use base64::Engine as _;
    use sha2::Digest as _;

    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-count-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(AddRequest {
            filename: Some("browser-count.bin".into()),
            page_url: Some("https://example.test/watch/browser-count".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..file_request(
                "https://example.test/browser-count.bin".into(),
                &root.join("out"),
            )
        })
        .unwrap()
        .remove(0);
    engine.shutdown();
    // Stands in for the desktop, which authorizes a job for a transfer, and for an exited sender,
    // which takes the authorization back.
    let authorize = |authorized: bool| {
        let mut store = Store::open(&paths.database).unwrap();
        let mut job = store
            .load_jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.id == id)
            .unwrap();
        job.request.start_at = None;
        job.state = JobState::Paused;
        job.browser_transfer_authorized = authorized;
        store.save_job(&job).unwrap();
    };
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let begin = |engine: &Engine, restart: bool| -> Response {
        dispatch(
            &paths,
            engine,
            Transfer::Begin {
                id: id.clone(),
                total: Some(4_000_000),
                etag: Some("\"browser-count\"".into()),
                restart,
                sender_pid: Some(std::process::id()),
                sender_started_at: Some(sender_started_at),
            },
        )
        .unwrap()
    };
    // The extension's side: a begin, then a chunk of each of `lengths` bytes, then the release.
    let transfer = |engine: &Engine, restart: bool, lengths: &[usize]| {
        let begun = begin(engine, restart);
        let token = begun.token.clone().unwrap();
        let mut stream = begun.streams[0].clone();
        for &length in lengths {
            let bytes = vec![7u8; length];
            stream = dispatch(
                &paths,
                engine,
                Transfer::Chunk {
                    id: id.clone(),
                    token: token.clone(),
                    stream_id: 0,
                    sequence: stream.next_sequence,
                    offset: stream.offset,
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    sha256: hex::encode(sha2::Sha256::digest(&bytes)),
                    client_checkpoint: None,
                },
            )
            .unwrap()
            .streams[0]
                .clone();
        }
        dispatch(
            &paths,
            engine,
            Transfer::Release {
                id: id.clone(),
                generation: begun.sender_generation.unwrap(),
            },
        )
        .unwrap();
    };

    authorize(true);
    let engine = Engine::open(paths.clone()).unwrap();
    transfer(&engine, false, &[300_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 300_000);

    // A begin that ends before its first chunk, and the one after it.
    transfer(&engine, false, &[]);
    transfer(&engine, false, &[100_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 400_000);

    // A begin, the app closed before its first chunk, and a begin in the next session.
    begin(&engine, false);
    engine.shutdown();
    let engine = Engine::open(paths.clone()).unwrap();
    transfer(&engine, false, &[100_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 500_000);

    // The authorization taken back, and the transfer begun again from a source refresh.
    engine.shutdown();
    authorize(false);
    let engine = Engine::open(paths.clone()).unwrap();
    engine.begin_source_refresh(&id).unwrap();
    transfer(&engine, false, &[100_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 600_000);

    // The bytes the receiver had committed are lost, so the transfer fetches them again.
    let work = engine
        .snapshot()
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap()
        .work_dir
        .clone()
        .unwrap();
    fs::remove_dir_all(work.join("browser-transfer")).unwrap();
    // A begin that ends before its first chunk leaves the job with the bytes the receiver has.
    transfer(&engine, false, &[]);
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!((job.state, job.downloaded), (JobState::Paused, 0));
    transfer(&engine, false, &[300_000, 300_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 600_000);

    // A restart downloads the file again from the first byte.
    transfer(&engine, true, &[200_000]);
    assert_eq!(consumed_quota_bytes(&engine, &queue), 800_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exited_native_sender_pauses_without_an_idle_timeout() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-owner-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let id = engine
        .add(AddRequest {
            url: "https://example.test/browser-owner.bin".into(),
            filename: Some("browser-owner.bin".into()),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    engine.shutdown();

    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    job.browser_transfer_authorized = true;
    store.save_job(&job).unwrap();
    drop(store);

    let mut sender = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let handle = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            sender.id(),
        )
    };
    assert!(!handle.is_null());
    let sender_started_at = process_started_at(handle).unwrap();
    unsafe {
        CloseHandle(handle);
    }

    let engine = Engine::open(paths).unwrap();
    let (_, generation) = engine
        .begin_browser_transfer(&id, sender.id(), sender_started_at, false)
        .unwrap();
    sender.kill().unwrap();
    sender.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while engine
        .snapshot()
        .jobs
        .iter()
        .find(|job| job.id == id)
        .is_some_and(|job| job.state != JobState::Paused)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(job.state, JobState::Paused);
    assert!(!job.browser_transfer_authorized);
    assert!(engine.browser_transfer_job(&id).is_err());
    assert!(engine
        .try_begin_browser_request(&id, generation, "late-request")
        .is_err());
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn windows_filename_rules_remove_traversal_and_devices() {
    assert_eq!(safe_filename("../../CON", "12345678"), "_CON");
    assert_eq!(safe_filename("a<b>c?.zip", "12345678"), "a_b_c_.zip");
    assert_eq!(safe_filename("...", "12345678"), "download-12345678");
    assert_eq!(
        safe_filename(&"a".repeat(220), "12345678").chars().count(),
        180
    );
    let long = safe_filename(&format!("{}.mp4", "b".repeat(300)), "12345678");
    assert_eq!(long.chars().count(), 180);
    assert!(long.ends_with(".mp4"));
    assert_eq!(safe_filename("com0.txt", "12345678"), "_com0.txt");
    assert_eq!(safe_filename("CONIN$", "12345678"), "_CONIN$");
}

#[test]
fn errors_redact_url_and_header_secrets() {
    let mut request = AddRequest {
        url: "https://example.test/file?token=secret".into(),
        ..Default::default()
    };
    request
        .headers
        .insert("Authorization".into(), "Bearer private".into());
    request.session_cookies.push(ScopedCookie {
        value: "cookie-private".into(),
        ..Default::default()
    });
    let error = safe_error(
        &request,
        "https://example.test/file?token=secret Bearer private cookie-private failed",
    );
    assert!(!error.contains("token=secret"));
    assert!(!error.contains("Bearer private"));
    assert!(!error.contains("cookie-private"));

    // A yt-dlp failure puts the requested selection in the argument list at the head and
    // the reason in the tail; both must survive the trim, or the log cannot explain the
    // failure the user saw (800p selection reported as "format is not available").
    let verbose = format!(
            "[debug] Command-line config: ['-f', 'bestvideo[height=800]+bestaudio', '--extractor-args', 'generic:impersonate'] {} ERROR: Requested format is not available. Use --list-formats",
            "diagnostic ".repeat(300)
        );
    let bounded = safe_error(&request, &verbose);
    assert!(bounded.contains("hata ayrıntısı kısaltıldı"));
    assert!(
        bounded.contains("height=800"),
        "the requested selection must survive"
    );
    assert!(bounded.contains("Requested format is not available"));
    assert!(
        bounded.chars().count() <= 3_100,
        "{}",
        bounded.chars().count()
    );

    request.page_url = Some("https://example.test/watch".into());
    let buried = format!(
        "{} HTTP Error 403: Forbidden {}",
        "leading diagnostic ".repeat(80),
        "trailing traceback ".repeat(80)
    );
    let failure = transfer_failure(&request, anyhow!(buried));
    // A 403 is a source-refresh condition, and the reason must survive into the message: the
    // report a user sends has to explain the failure. User-facing guidance comes from the job
    // phase ("kaynak adresinin yenilenmesi gerekiyor"), not from hiding the error text.
    assert!(failure.source_refresh);
    assert!(failure.message.contains("HTTP Error 403: Forbidden"));
}

#[test]
fn proxy_tunnel_failures_are_retryable() {
    // Reproduces the observed fragment-stage failure: the local proxy
    // answered CONNECT with 503 and the child's urllib stack reported it
    // verbatim; the job must stay retryable, not fail permanently.
    let tunnel = anyhow!(
        "medya parçası aşamasında medya indirmesi başarısız: [download] Got error: \
             ('Unable to connect to proxy', OSError('Tunnel connection failed: \
             503 Service Unavailable')). Retrying (1/3)..."
    );
    assert!(retryable_error(&tunnel));
    assert!(retryable_error(&anyhow!(
        "Unable to connect to proxy: timed out"
    )));
    assert!(retryable_error(&anyhow!(
        "HTTP Error 503: Service Unavailable"
    )));
    assert!(!retryable_error(&anyhow!("HTTP Error 403: Forbidden")));
    assert!(!retryable_error(&anyhow!("Unsupported URL")));
}

#[test]
fn an_http_error_in_a_media_tool_message_is_retried_by_the_rule_of_a_typed_status() {
    for status in 100..=599 {
        let typed = anyhow::Error::new(transfer::HttpStatusError { status });
        let media = anyhow!("ERROR: unable to download video data: HTTP Error {status}: Whatever");
        assert_eq!(
            retryable_error(&media),
            retryable_error(&typed),
            "HTTP {status}"
        );
    }
    // The CDN statuses that no fixed list names are transient; 501 and client errors are not.
    assert!(retryable_error(&anyhow!(
        "HTTP Error 522: Connection Timed Out"
    )));
    assert!(!retryable_error(&anyhow!(
        "HTTP Error 501: Not Implemented"
    )));
    assert!(!retryable_error(&anyhow!("HTTP Error 404: Not Found")));
    // Only a three-digit number is a status.
    assert!(!retryable_error(&anyhow!("HTTP Error 5039: unexpected")));
}

#[test]
fn a_file_locked_by_another_program_is_retried_but_other_access_errors_are_not() {
    let locked = |code| {
        anyhow::Error::new(std::io::Error::from_raw_os_error(code))
            .context("Tamamlanan dosya güvenle taşınamadı")
    };
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION
    assert!(retryable_error(&locked(32)));
    assert!(retryable_error(&locked(33)));
    // ERROR_ACCESS_DENIED stays permanent
    assert!(!retryable_error(&locked(5)));
}

#[test]
fn a_job_whose_destination_folder_was_deleted_recreates_it_before_starting() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-folder-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    let folder = root.join("deleted-after-queueing");
    let output = folder.join("file.bin");
    let id = Uuid::new_v4().to_string();
    // A port nothing listens on, so the transfer itself fails at once after the folder step.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/file.bin", closed.local_addr().unwrap());
    drop(closed);
    let job = Job {
        id: id.clone(),
        request: AddRequest {
            url,
            kind: DownloadKind::File,
            ..Default::default()
        },
        name: "file.bin".into(),
        state: JobState::Queued,
        path: output.clone(),
        downloaded: 0,
        total: None,
        speed: 0,
        eta: None,
        error: None,
        phase: "Sırada".into(),
        created_at: 0,
        updated_at: 0,
        attempts: 0,
        work_dir: Some(crate::recovery::work_dir(&output, &id)),
        remove_requested: None,
        claim: None,
        legacy_completed: false,
        browser_transfer_authorized: false,
        priority: 0,
        force_start: false,
        open_when_done: false,
        live: false,
        sequence: 0,
    };
    let allocation = ConnectionAllocation {
        direct: 1,
        media_fragments: 0,
        usage_modes: UsageModes::default(),
    };
    let error = run_job(
        &paths,
        &job,
        &output,
        &TransferControl::default(),
        &NetworkGovernor::new(2),
        allocation,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        folder.is_dir(),
        "the destination folder was not recreated: {error:#}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_refuses_a_redirect_to_a_non_http_protocol() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 2048];
        let _ = std::io::Read::read(&mut stream, &mut buffer);
        std::io::Write::write_all(
            &mut stream,
            b"HTTP/1.1 302 Found\r\nLocation: ftp://127.0.0.1:1/mirror.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    });
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("http://{address}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        ..Default::default()
    };
    let error = perform_synchronization(&policy, &NetworkGovernor::new(4))
        .err()
        .expect("the ftp redirect must be refused");
    server.join().unwrap();
    assert!(
        error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<curl::Error>())
            .any(|error| error.is_unsupported_protocol()),
        "{error:#}"
    );
    assert!(!root.join("mirror.bin").exists());
    fs::remove_dir_all(root).unwrap();
}

/// Serves one request: `Not Modified` when it carries a validator, otherwise `body`.
/// Returns the base address and a handle that yields the request head.
fn serve_sync_once(body: &'static [u8]) -> (String, thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut buffer = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = std::io::Read::read(&mut stream, &mut buffer).unwrap();
            assert!(count > 0, "the client closed before sending its request");
            head.extend_from_slice(&buffer[..count]);
        }
        let head = String::from_utf8(head).unwrap();
        let lower = head.to_ascii_lowercase();
        let reply = if lower.contains("if-none-match:") || lower.contains("if-modified-since:") {
            b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n".to_vec()
        } else {
            let mut reply = format!(
                "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            reply.extend_from_slice(body);
            reply
        };
        std::io::Write::write_all(&mut stream, &reply).unwrap();
        head
    });
    (format!("http://{address}"), server)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn synchronization_restores_a_deleted_destination_instead_of_trusting_the_validators() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let (base, server) = serve_sync_once(b"mirror content");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        etag: Some("\"v1\"".into()),
        last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
        content_sha256: Some(sha256_hex(b"mirror content")),
        ..Default::default()
    };
    let outcome = perform_synchronization(&policy, &NetworkGovernor::new(4)).unwrap();
    let head = server.join().unwrap().to_ascii_lowercase();
    assert!(!head.contains("if-none-match"), "{head}");
    assert!(!head.contains("if-modified-since"), "{head}");
    assert!(outcome.changed);
    assert_eq!(
        fs::read(root.join("mirror.bin")).unwrap(),
        b"mirror content"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_still_sends_validators_while_the_destination_exists() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirror.bin"), b"mirror content").unwrap();
    let (base, server) = serve_sync_once(b"mirror content");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        etag: Some("\"v1\"".into()),
        content_sha256: Some(sha256_hex(b"mirror content")),
        ..Default::default()
    };
    let outcome = perform_synchronization(&policy, &NetworkGovernor::new(4)).unwrap();
    let head = server.join().unwrap().to_ascii_lowercase();
    assert!(head.contains("if-none-match: \"v1\""), "{head}");
    assert!(!outcome.changed);
    assert_eq!(
        fs::read(root.join("mirror.bin")).unwrap(),
        b"mirror content"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_keeps_a_good_file_when_the_server_answers_with_an_empty_body() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirror.bin"), b"good content").unwrap();
    let (base, server) = serve_sync_once(b"");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        content_sha256: Some(sha256_hex(b"good content")),
        ..Default::default()
    };
    perform_synchronization(&policy, &NetworkGovernor::new(4))
        .err()
        .expect("an empty body must not replace a good file");
    server.join().unwrap();
    assert_eq!(fs::read(root.join("mirror.bin")).unwrap(), b"good content");
    let leftovers = fs::read_dir(&root).unwrap().count();
    assert_eq!(leftovers, 1, "the temporary file was removed");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_keeps_a_good_file_from_an_empty_body_when_no_digest_is_stored() {
    // A new policy, or one whose address or destination was edited, has no stored digest.
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirror.bin"), b"good content").unwrap();
    let (base, server) = serve_sync_once(b"");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        content_sha256: None,
        ..Default::default()
    };
    perform_synchronization(&policy, &NetworkGovernor::new(4))
        .err()
        .expect("an empty body must not replace a good file");
    server.join().unwrap();
    assert_eq!(fs::read(root.join("mirror.bin")).unwrap(), b"good content");
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_restores_a_deleted_destination_from_an_empty_body() {
    // Nothing is left to keep, so the refusal that protects a good file does not apply.
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let (base, server) = serve_sync_once(b"");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        etag: Some("\"v1\"".into()),
        content_sha256: Some(sha256_hex(b"good content")),
        ..Default::default()
    };
    let outcome = perform_synchronization(&policy, &NetworkGovernor::new(4)).unwrap();
    server.join().unwrap();
    assert!(outcome.changed);
    assert_eq!(fs::read(root.join("mirror.bin")).unwrap(), b"");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn synchronization_accepts_an_empty_body_when_the_file_was_empty_before() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-sync-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("mirror.bin"), b"").unwrap();
    let (base, server) = serve_sync_once(b"");
    let policy = SyncPolicy {
        id: Uuid::new_v4().to_string(),
        url: format!("{base}/mirror.bin"),
        destination: root.join("mirror.bin"),
        enabled: true,
        interval_minutes: 60,
        overwrite: true,
        content_sha256: Some(sha256_hex(b"")),
        ..Default::default()
    };
    let outcome = perform_synchronization(&policy, &NetworkGovernor::new(4)).unwrap();
    server.join().unwrap();
    assert!(!outcome.changed);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_stored_profile_with_two_spellings_of_one_login_host_keeps_the_first_and_saves() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-logins-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    Engine::open(paths.clone()).unwrap().shutdown();
    let login = |host: &str, username: &str| crate::model::SiteLogin {
        host: host.into(),
        username: username.into(),
        password: String::new(),
        allow_http: false,
    };
    let mut store = Store::open(&paths.database).unwrap();
    let mut stored = store.load_settings().unwrap().unwrap();
    stored.site_logins = vec![
        login("bücher.example", "first"),
        login("files.example.com", "other"),
        login("xn--bcher-kva.example", "second"),
    ];
    store.save_settings(&stored).unwrap();
    drop(store);

    let engine = Engine::open(paths.clone()).unwrap();
    let snapshot = engine.snapshot();
    assert!(snapshot.warning.is_some());
    assert_eq!(
        snapshot.settings.site_logins,
        [
            login("bücher.example", "first"),
            login("files.example.com", "other")
        ]
    );
    crate::validation::validate_settings(&snapshot.settings).unwrap();
    engine.update_settings(snapshot.settings.clone()).unwrap();
    engine.shutdown();

    let reopened = Engine::open(paths).unwrap();
    assert_eq!(
        reopened.snapshot().settings.site_logins,
        snapshot.settings.site_logins
    );
    reopened.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Leaves a stored profile as a build without the defaults marker wrote it.
fn store_profile_without_defaults_marker(paths: &AppPaths, connections: u8, per_host_limit: u8) {
    Engine::open(paths.clone()).unwrap().shutdown();
    let mut store = Store::open(&paths.database).unwrap();
    let mut value = serde_json::to_value(store.load_settings().unwrap().unwrap()).unwrap();
    let fields = value.as_object_mut().unwrap();
    fields.remove("defaults_version");
    fields.insert("connections".into(), connections.into());
    fields.insert("per_host_limit".into(), per_host_limit.into());
    store
        .save_settings(&serde_json::from_value(value).unwrap())
        .unwrap();
}

fn connection_limits(engine: &Engine) -> (u8, u8) {
    let settings = engine.snapshot().settings.clone();
    (settings.connections, settings.per_host_limit)
}

#[test]
fn a_new_profile_starts_with_eight_connections_per_file_and_per_host() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-defaults-{}", Uuid::new_v4()));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    assert_eq!(connection_limits(&engine), (8, 8));
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_stored_profile_with_the_old_default_pair_moves_to_eight_and_eight_once() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-defaults-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    store_profile_without_defaults_marker(&paths, 4, 2);

    let engine = Engine::open(paths.clone()).unwrap();
    assert_eq!(connection_limits(&engine), (8, 8));
    let saved = Store::open(&paths.database)
        .unwrap()
        .load_settings()
        .unwrap()
        .unwrap();
    assert_eq!((saved.connections, saved.per_host_limit), (8, 8));
    assert_eq!(saved.defaults_version, 1);
    engine.shutdown();

    let reopened = Engine::open(paths).unwrap();
    assert_eq!(connection_limits(&reopened), (8, 8));
    reopened.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_stored_profile_moves_each_field_with_its_old_default_and_keeps_the_others() {
    for (stored, expected) in [
        ((4, 2), (8, 8)),
        ((16, 2), (16, 8)),
        ((4, 5), (8, 5)),
        ((2, 2), (2, 8)),
        ((3, 6), (3, 6)),
        ((8, 2), (8, 8)),
        ((2, 4), (2, 4)),
        ((1, 1), (1, 1)),
        ((16, 16), (16, 16)),
    ] {
        let root =
            std::env::temp_dir().join(format!("ssdownload-engine-defaults-{}", Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        store_profile_without_defaults_marker(&paths, stored.0, stored.1);
        let engine = Engine::open(paths).unwrap();
        assert_eq!(connection_limits(&engine), expected, "stored {stored:?}");
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn four_and_two_chosen_after_the_move_stay_through_restarts() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-defaults-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    store_profile_without_defaults_marker(&paths, 4, 2);

    let engine = Engine::open(paths.clone()).unwrap();
    assert_eq!(connection_limits(&engine), (8, 8));
    let mut chosen = engine.snapshot().settings.clone();
    chosen.connections = 4;
    chosen.per_host_limit = 2;
    engine.update_settings(chosen.clone()).unwrap();
    engine.shutdown();

    let engine = Engine::open(paths.clone()).unwrap();
    assert_eq!(connection_limits(&engine), (4, 2));
    // A settings file or a stale dialog snapshot that carries no marker does not reopen the move.
    let mut value = serde_json::to_value(&chosen).unwrap();
    value.as_object_mut().unwrap().remove("defaults_version");
    engine
        .update_settings(serde_json::from_value(value).unwrap())
        .unwrap();
    engine.shutdown();

    let engine = Engine::open(paths).unwrap();
    assert_eq!(connection_limits(&engine), (4, 2));
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_named_from_the_address_gets_the_decoded_file_name() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-name-{}", Uuid::new_v4()));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(AddRequest {
            url: "https://example.test/files/Y%C4%B1ll%C4%B1k%20Rapor%20%281%29.pdf?v=a%20b".into(),
            directory: Some(root.join("out")),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..Default::default()
        })
        .unwrap()
        .remove(0);
    let snapshot = engine.snapshot();
    let job = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(job.name, "Yıllık Rapor (1).pdf");
    assert_eq!(job.path.file_name().unwrap(), "Yıllık Rapor (1).pdf");
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

fn job_state(engine: &Engine, id: &str) -> JobState {
    engine
        .snapshot()
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap()
        .state
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Serves fixed bodies on 127.0.0.1. A gated path sends its body only after `release`, which
/// keeps its download running for as long as a test needs; `hits` lists the GET paths served.
struct FileServer {
    address: std::net::SocketAddr,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    hits: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

type ServedFile = (String, Vec<u8>, bool);

impl FileServer {
    fn start(files: Vec<ServedFile>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let files = Arc::new(files);
        let release = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let (release, stop, hits) = (release.clone(), stop.clone(), hits.clone());
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (files, release, stop, hits) =
                                (files.clone(), release.clone(), stop.clone(), hits.clone());
                            handlers.push(thread::spawn(move || {
                                serve_connection(stream, &files, &release, &stop, &hits)
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
                for handler in handlers {
                    let _ = handler.join();
                }
            })
        };
        Self {
            address,
            release,
            stop,
            hits,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn release(&self) {
        self.release.store(true, Ordering::Release);
    }
}

impl Drop for FileServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_connection(
    mut stream: std::net::TcpStream,
    files: &[ServedFile],
    release: &AtomicBool,
    stop: &AtomicBool,
    hits: &Mutex<Vec<String>>,
) {
    use std::io::{Read as _, Write as _};
    stream.set_nonblocking(false).unwrap();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 512];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => request.extend_from_slice(&buffer[..count]),
        }
    }
    let head = String::from_utf8_lossy(&request);
    let mut words = head.split_whitespace();
    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let Some((_, body, gated)) = files.iter().find(|(name, _, _)| name == path) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    if method != "GET" {
        return;
    }
    hits.lock().unwrap().push(path.to_owned());
    while *gated && !release.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = stream.write_all(body);
}

fn file_request(url: String, directory: &Path) -> AddRequest {
    AddRequest {
        url,
        kind: DownloadKind::File,
        directory: Some(directory.to_path_buf()),
        ..Default::default()
    }
}

#[test]
fn the_size_of_a_result_that_followed_processing_adds_nothing_to_a_quota() {
    // A download that fetched 110 bytes and converted them into 673 has counted what it received.
    assert_eq!(uncounted_result_bytes(JobState::Processing, 110, 673), 0);
    // A download whose last progress report was lost has the rest of its result left to count.
    assert_eq!(
        uncounted_result_bytes(JobState::Downloading, 600, 1000),
        400
    );
    assert_eq!(uncounted_result_bytes(JobState::Downloading, 1000, 700), 0);
}

/// Gives the default queue a quota that is never reached and returns the queue's id.
fn give_the_default_queue_a_quota(engine: &Engine) -> String {
    let queue = engine.snapshot().settings.queues[0].clone();
    engine
        .update_queue_policy(QueuePolicy {
            quota: Some(QueueQuota {
                limit_bytes: 1 << 30,
                ..Default::default()
            }),
            ..queue.clone()
        })
        .unwrap();
    queue.id
}

fn consumed_quota_bytes(engine: &Engine, queue: &str) -> u64 {
    let settings = engine.snapshot().settings;
    settings
        .queues
        .iter()
        .find(|candidate| candidate.id == queue)
        .and_then(|candidate| candidate.quota.as_ref())
        .unwrap()
        .consumed_bytes
}

#[test]
fn a_file_download_adds_its_size_to_the_quota_of_its_queue_once() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-quota-{}", Uuid::new_v4()));
    let body = vec![7u8; 300_000];
    let server = FileServer::start(vec![("/quota.bin".into(), body.clone(), false)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(file_request(server.url("/quota.bin"), &root.join("out")))
        .unwrap()
        .remove(0);

    wait_until("the download", || {
        job_state(&engine, &id) == JobState::Completed
    });

    assert_eq!(consumed_quota_bytes(&engine, &queue), body.len() as u64);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Ends a job that waits for its body the way a worker ends it: with `reports`, then a result of
/// `result_bytes`. Returns the bytes its queue's quota has consumed. A worker's generation is not
/// visible, so every message goes out for each generation the job can have; the engine ignores
/// the ones that are not its worker's.
fn quota_consumed_by_a_job_that_reports(reports: &[TransferProgress], result_bytes: u64) -> u64 {
    const GENERATIONS: std::ops::RangeInclusive<u64> = 1..=8;
    let root = std::env::temp_dir().join(format!("ssdownload-engine-quota-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/held.bin".into(), vec![5; 4096], true)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(file_request(server.url("/held.bin"), &root.join("out")))
        .unwrap()
        .remove(0);
    wait_until("the worker to wait for the body", || {
        server
            .hits
            .lock()
            .unwrap()
            .iter()
            .any(|hit| hit == "/held.bin")
    });
    let path = engine
        .snapshot()
        .jobs
        .iter()
        .find(|job| job.id == id)
        .unwrap()
        .path
        .clone();

    let send = |event| engine.inner.sender.send(Message::Worker(event)).unwrap();
    for generation in GENERATIONS {
        for report in reports {
            send(WorkerEvent::Progress {
                id: id.clone(),
                generation,
                value: report.clone(),
            });
        }
    }
    for generation in GENERATIONS {
        send(WorkerEvent::Finished {
            id: id.clone(),
            generation,
            result: Ok(TransferResult {
                path: path.clone(),
                bytes: result_bytes,
                warning: None,
            }),
        });
    }
    // The engine waits for the real worker to end before it takes the result.
    server.release();
    wait_until("the job", || job_state(&engine, &id) == JobState::Completed);

    let consumed = consumed_quota_bytes(&engine, &queue);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
    consumed
}

#[test]
fn a_job_that_reported_processing_adds_nothing_when_its_larger_result_arrives() {
    let converting = TransferProgress {
        downloaded: 110_000,
        processing: true,
        ..TransferProgress::default()
    };

    assert_eq!(
        quota_consumed_by_a_job_that_reports(&[converting], 673_000),
        110_000
    );
}

#[test]
fn a_job_that_did_not_report_everything_gets_the_rest_of_its_result_added() {
    let downloading = TransferProgress {
        downloaded: 600_000,
        ..TransferProgress::default()
    };

    assert_eq!(
        quota_consumed_by_a_job_that_reports(&[downloading], 1_000_000),
        1_000_000
    );
}

fn downloaded(bytes: &[u64]) -> Vec<TransferProgress> {
    bytes
        .iter()
        .map(|&downloaded| TransferProgress {
            downloaded,
            ..TransferProgress::default()
        })
        .collect()
}

/// Sends the event `event` makes for each generation a job's worker can have. A worker's
/// generation is not visible; the engine ignores the events that are not its worker's.
fn send_to_the_worker(engine: &Engine, event: &dyn Fn(u64) -> WorkerEvent) {
    for generation in 1..=8 {
        engine
            .inner
            .sender
            .send(Message::Worker(event(generation)))
            .unwrap();
    }
}

/// Reports each of `bytes` as the job's downloaded size, as its worker does.
fn report_downloaded(engine: &Engine, id: &str, bytes: &[u64]) {
    for value in downloaded(bytes) {
        send_to_the_worker(engine, &|generation| WorkerEvent::Progress {
            id: id.to_owned(),
            generation,
            value: value.clone(),
        });
    }
}

#[test]
fn bytes_a_job_reports_again_after_its_count_fell_are_counted_once() {
    assert_eq!(
        quota_consumed_by_a_job_that_reports(&downloaded(&[600_000, 400_000, 700_000]), 700_000),
        700_000
    );
}

#[test]
fn a_media_run_that_starts_again_counts_only_the_bytes_beyond_where_the_last_run_got() {
    // yt-dlp, started again (a new speed limit, a stall, a lost fragment), first reports the files
    // it has finished, then the partial file it resumes, then what it fetches on.
    let first_run: [u64; 3] = [200_000, 350_000, 500_000];
    let second_run: [u64; 4] = [200_000, 500_000, 650_000, 800_000];
    let reports = downloaded(&[&first_run[..], &second_run[..]].concat());
    assert_eq!(
        quota_consumed_by_a_job_that_reports(&reports, 800_000),
        800_000
    );
}

#[test]
fn every_run_of_a_live_recording_counts_in_full() {
    // A stopped live recording cannot continue its file, so the run that resumes it records new
    // content. The reports stand in for yt-dlp's, which start every run of a recording at zero.
    let root = std::env::temp_dir().join(format!("ssdownload-engine-quota-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/live.bin".into(), vec![4; 4096], true)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue = give_the_default_queue_a_quota(&engine);
    let id = engine
        .add(AddRequest {
            start_at: Some(Utc::now().timestamp() + 3600),
            ..file_request(server.url("/live.bin"), &root.join("out"))
        })
        .unwrap()
        .remove(0);
    mark_as_live_recording(&id);
    let runs = || server.hits.lock().unwrap().len();
    engine.start_now(&id).unwrap();
    wait_until("the first run", || runs() == 1);
    report_downloaded(&engine, &id, &[200_000, 500_000]);
    engine.pause(&id).unwrap();
    assert_eq!(consumed_quota_bytes(&engine, &queue), 500_000);

    engine.resume(&id).unwrap();
    wait_until("the second run", || runs() == 2);
    report_downloaded(&engine, &id, &[0, 100_000, 300_000]);
    server.release();
    wait_until("the recording to end", || {
        job_state(&engine, &id) == JobState::Completed
    });

    assert_eq!(consumed_quota_bytes(&engine, &queue), 800_000);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

fn shutdown_when_done(queue: &QueuePolicy) -> QueuePolicy {
    QueuePolicy {
        // The countdown is far longer than any test, and the tests cancel it, so
        // no shutdown is ever issued.
        completion: CompletionAction::ShutdownComputer {
            countdown_seconds: 3600,
        },
        ..queue.clone()
    }
}

#[test]
fn a_queue_that_finished_first_starts_its_shutdown_when_the_last_other_job_completes() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-idle-a-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![
        ("/fast.bin".into(), vec![1; 4096], false),
        ("/slow.bin".into(), vec![2; 4096], true),
    ]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue_a = engine.snapshot().settings.queues[0].clone();
    let queue_b = engine.create_queue("B".into()).unwrap();
    engine
        .update_queue_policy(shutdown_when_done(&queue_a))
        .unwrap();
    let out = root.join("out");
    let slow = engine
        .add(AddRequest {
            queue_id: Some(queue_b),
            ..file_request(server.url("/slow.bin"), &out)
        })
        .unwrap()
        .remove(0);
    let fast = engine
        .add(file_request(server.url("/fast.bin"), &out))
        .unwrap()
        .remove(0);

    wait_until("the fast job", || {
        job_state(&engine, &fast) == JobState::Completed
    });
    assert!(job_state(&engine, &slow).is_active());
    assert!(engine.snapshot().completion_countdown.is_none());

    server.release();
    wait_until("the countdown", || {
        engine.snapshot().completion_countdown.is_some()
    });
    assert_eq!(job_state(&engine, &slow), JobState::Completed);
    let countdown = engine.snapshot().completion_countdown.unwrap();
    assert_eq!(countdown.queue_id, queue_a.id);
    engine.cancel_completion_action(queue_a.id).unwrap();
    assert!(engine.snapshot().completion_countdown.is_none());
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn removing_the_last_job_that_blocks_a_finished_queue_starts_its_shutdown() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-idle-b-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/fast.bin".into(), vec![1; 4096], false)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue_a = engine.snapshot().settings.queues[0].clone();
    let queue_b = engine.create_queue("B".into()).unwrap();
    engine
        .update_queue_policy(shutdown_when_done(&queue_a))
        .unwrap();
    let out = root.join("out");
    let waiting = engine
        .add(AddRequest {
            queue_id: Some(queue_b),
            start_at: Some(Utc::now().timestamp() + 3600),
            ..file_request("https://waiting.example/file.bin".into(), &out)
        })
        .unwrap()
        .remove(0);
    let fast = engine
        .add(file_request(server.url("/fast.bin"), &out))
        .unwrap()
        .remove(0);

    wait_until("the fast job", || {
        job_state(&engine, &fast) == JobState::Completed
    });
    assert!(engine.snapshot().completion_countdown.is_none());

    engine.remove(&waiting, false).unwrap();
    let countdown = engine.snapshot().completion_countdown.unwrap();
    assert_eq!(countdown.queue_id, queue_a.id);
    engine.cancel_completion_action(queue_a.id).unwrap();
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

/// Adds a queue with a chosen id. The queues of a pass are evaluated in id order, so
/// "a-countdown" is always evaluated before the default queue.
fn add_queue(engine: &Engine, id: &str, completion: CompletionAction) {
    let mut settings = engine.snapshot().settings;
    settings.queues.push(QueuePolicy {
        id: id.into(),
        name: id.into(),
        completion,
        ..Default::default()
    });
    engine.update_settings(settings).unwrap();
}

fn queued_in(url: String, directory: &Path, queue: &str) -> AddRequest {
    AddRequest {
        queue_id: Some(queue.into()),
        ..file_request(url, directory)
    }
}

#[test]
fn moving_a_finished_job_into_a_queue_does_not_arm_that_queue_s_shutdown() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-move-done-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/done.bin".into(), vec![1; 4096], false)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let default_queue = engine.snapshot().settings.queues[0].clone();
    let source = engine.create_queue("Source".into()).unwrap();
    let unrelated = engine.create_queue("Unrelated".into()).unwrap();
    engine
        .update_queue_policy(shutdown_when_done(&default_queue))
        .unwrap();
    let out = root.join("out");
    let finished = engine
        .add(queued_in(server.url("/done.bin"), &out, &source))
        .unwrap()
        .remove(0);
    let waiting = engine
        .add(AddRequest {
            start_at: Some(Utc::now().timestamp() + 3600),
            ..queued_in("https://waiting.example/file.bin".into(), &out, &unrelated)
        })
        .unwrap()
        .remove(0);
    wait_until("the finished job", || {
        job_state(&engine, &finished) == JobState::Completed
    });

    engine
        .move_to_queue(vec![finished], default_queue.id.clone())
        .unwrap();
    engine.remove(&waiting, false).unwrap();

    // The default queue holds nothing but a job that finished elsewhere, so its shutdown has
    // no work of its own to follow. A countdown is running exactly when cancelling succeeds.
    assert!(engine.cancel_completion_action(default_queue.id).is_err());
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_notify_queue_is_not_held_back_by_a_countdown_that_starts_in_the_same_pass() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-engine-notify-gate-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![
        ("/fast.bin".into(), vec![1; 4096], false),
        ("/slow.bin".into(), vec![2; 4096], true),
    ]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    add_queue(
        &engine,
        "a-countdown",
        CompletionAction::ShutdownComputer {
            countdown_seconds: 3600,
        },
    );
    let default_queue = engine.snapshot().settings.queues[0].clone();
    engine
        .update_queue_policy(QueuePolicy {
            completion: CompletionAction::Notify,
            ..default_queue.clone()
        })
        .unwrap();
    let out = root.join("out");
    let fast = engine
        .add(queued_in(server.url("/fast.bin"), &out, "a-countdown"))
        .unwrap()
        .remove(0);
    engine
        .add(queued_in(server.url("/slow.bin"), &out, &default_queue.id))
        .unwrap();
    wait_until("the fast job", || {
        job_state(&engine, &fast) == JobState::Completed
    });

    // The slow job is the last to finish: the countdown queue starts first in id order and
    // the Notify queue is evaluated right after it, in the same pass.
    server.release();
    wait_until("the countdown", || {
        engine.snapshot().completion_countdown.is_some()
    });
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.completion_countdown.unwrap().queue_id,
        "a-countdown"
    );
    assert!(
        snapshot
            .completion_events
            .iter()
            .any(|event| event.queue_id == default_queue.id),
        "{:?}",
        snapshot.completion_events
    );
    engine
        .cancel_completion_action("a-countdown".into())
        .unwrap();
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelling_a_countdown_also_cancels_the_actions_waiting_behind_it() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-engine-cancel-next-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![
        ("/fast.bin".into(), vec![1; 4096], false),
        ("/slow.bin".into(), vec![2; 4096], true),
    ]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let shutdown = CompletionAction::ShutdownComputer {
        countdown_seconds: 3600,
    };
    add_queue(&engine, "a-countdown", shutdown);
    let default_queue = engine.snapshot().settings.queues[0].clone();
    engine
        .update_queue_policy(shutdown_when_done(&default_queue))
        .unwrap();
    let out = root.join("out");
    let fast = engine
        .add(queued_in(server.url("/fast.bin"), &out, "a-countdown"))
        .unwrap()
        .remove(0);
    engine
        .add(queued_in(server.url("/slow.bin"), &out, &default_queue.id))
        .unwrap();
    wait_until("the fast job", || {
        job_state(&engine, &fast) == JobState::Completed
    });
    server.release();
    wait_until("the countdown", || {
        engine.snapshot().completion_countdown.is_some()
    });
    assert_eq!(
        engine.snapshot().completion_countdown.unwrap().queue_id,
        "a-countdown"
    );

    engine
        .cancel_completion_action("a-countdown".into())
        .unwrap();

    // The default queue also finished and was waiting behind the countdown; it is cancelled
    // with it instead of starting in its place.
    assert!(engine.snapshot().completion_countdown.is_none());
    assert!(engine.cancel_completion_action(default_queue.id).is_err());
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn moving_a_finished_job_into_its_own_queue_keeps_that_queue_s_shutdown() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-own-queue-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![
        ("/fast.bin".into(), vec![1; 4096], false),
        ("/slow.bin".into(), vec![2; 4096], true),
    ]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let queue = engine.snapshot().settings.queues[0].clone();
    engine
        .update_queue_policy(shutdown_when_done(&queue))
        .unwrap();
    let out = root.join("out");
    let done = engine
        .add(file_request(server.url("/fast.bin"), &out))
        .unwrap()
        .remove(0);
    engine
        .add(file_request(server.url("/slow.bin"), &out))
        .unwrap();
    wait_until("the fast job", || {
        job_state(&engine, &done) == JobState::Completed
    });

    engine.move_to_queue(vec![done], queue.id.clone()).unwrap();
    server.release();

    wait_until(
        "the shutdown countdown of the queue that still had a running job",
        || engine.snapshot().completion_countdown.is_some(),
    );
    engine.cancel_completion_action(queue.id).unwrap();
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_expired_countdown_starts_the_next_queue_that_was_waiting_for_it() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-expiry-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![
        ("/fast.bin".into(), vec![1; 4096], false),
        ("/slow.bin".into(), vec![2; 4096], true),
    ]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    add_queue(
        &engine,
        "a-countdown",
        CompletionAction::RunProgram {
            program: PathBuf::from(std::env::var_os("COMSPEC").unwrap()),
            arguments: vec!["/C".into(), "exit".into()],
            countdown_seconds: 0,
        },
    );
    let default_queue = engine.snapshot().settings.queues[0].clone();
    engine
        .update_queue_policy(shutdown_when_done(&default_queue))
        .unwrap();
    let out = root.join("out");
    let fast = engine
        .add(queued_in(server.url("/fast.bin"), &out, "a-countdown"))
        .unwrap()
        .remove(0);
    engine
        .add(queued_in(server.url("/slow.bin"), &out, &default_queue.id))
        .unwrap();
    wait_until("the fast job", || {
        job_state(&engine, &fast) == JobState::Completed
    });

    server.release();
    wait_until("the second countdown", || {
        engine
            .snapshot()
            .completion_countdown
            .is_some_and(|countdown| countdown.queue_id == default_queue.id)
    });
    assert!(engine
        .snapshot()
        .completion_events
        .iter()
        .any(|event| event.queue_id == "a-countdown"));
    engine.cancel_completion_action(default_queue.id).unwrap();
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn jobs_added_in_the_same_second_keep_the_order_they_were_added_in() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-order-{}", Uuid::new_v4()));
    let paths = AppPaths::new(root.clone()).unwrap();
    let out = root.join("out");
    let future = Utc::now().timestamp() + 3600;
    let add = |engine: &Engine, index: usize| {
        engine
            .add(AddRequest {
                start_at: Some(future),
                ..file_request(format!("https://order.example/file-{index}.bin"), &out)
            })
            .unwrap()
            .remove(0)
    };
    let listed = |engine: &Engine| {
        engine
            .snapshot()
            .jobs
            .iter()
            .map(|job| job.id.clone())
            .collect::<Vec<_>>()
    };

    let engine = Engine::open(paths.clone()).unwrap();
    let mut added = (0..16).map(|index| add(&engine, index)).collect::<Vec<_>>();
    assert_eq!(listed(&engine), added);
    engine.shutdown();

    let engine = Engine::open(paths).unwrap();
    assert_eq!(listed(&engine), added);
    added.push(add(&engine, 16));
    assert_eq!(listed(&engine), added);

    engine
        .move_before(vec![added[16].clone()], added[0].clone())
        .unwrap();
    let moved = added.pop().unwrap();
    added.insert(0, moved);
    assert_eq!(listed(&engine), added);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn queued_jobs_start_in_the_order_they_were_added_in() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-start-{}", Uuid::new_v4()));
    let server = FileServer::start(
        (0..12)
            .map(|index| (format!("/file-{index}.bin"), vec![0; 1024], true))
            .collect(),
    );
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.max_active = 1;
    engine.update_settings(settings).unwrap();
    let mut queue = engine.snapshot().settings.queues[0].clone();
    queue.concurrency = 1;
    queue.enabled = false;
    engine.update_queue_policy(queue.clone()).unwrap();
    let out = root.join("out");
    for index in 0..12 {
        engine
            .add(file_request(
                server.url(&format!("/file-{index}.bin")),
                &out,
            ))
            .unwrap();
    }
    assert!(engine
        .snapshot()
        .jobs
        .iter()
        .all(|job| job.state == JobState::Scheduled));

    queue.enabled = true;
    engine.update_queue_policy(queue).unwrap();
    wait_until("the first download", || {
        !server.hits.lock().unwrap().is_empty()
    });
    assert_eq!(server.hits.lock().unwrap()[0], "/file-0.bin");
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pausing_a_browser_transfer_releases_the_permits_of_its_open_requests() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-browser-permits-{}",
        Uuid::new_v4()
    ));
    let paths = AppPaths::new(root.clone()).unwrap();
    let engine = Engine::open(paths.clone()).unwrap();
    let mut settings = engine.snapshot().settings;
    settings.experimental_browser_transfer = true;
    engine.update_settings(settings).unwrap();
    let id = engine
        .add(AddRequest {
            start_at: Some(Utc::now().timestamp() + 3600),
            ..file_request("https://example.test/permits.bin".into(), &root.join("out"))
        })
        .unwrap()
        .remove(0);
    engine.shutdown();

    let mut store = Store::open(&paths.database).unwrap();
    let mut job = store
        .load_jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    job.request.start_at = None;
    job.state = JobState::Paused;
    job.browser_transfer_authorized = true;
    store.save_job(&job).unwrap();
    drop(store);

    let engine = Engine::open(paths).unwrap();
    let sender_started_at =
        process_started_at(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
            .unwrap();
    let (_, generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at, false)
        .unwrap();
    let limit = usize::from(engine.snapshot().settings.per_host_limit);
    for index in 0..limit {
        assert!(engine
            .try_begin_browser_request(&id, generation, &format!("relay-{index}"))
            .unwrap());
    }
    let governor = engine.network_governor();
    assert!(
        governor.try_acquire_wildcard().is_none(),
        "the open requests hold every permit"
    );

    engine.pause(&id).unwrap();

    let permits = (0..limit)
        .map(|_| governor.try_acquire_wildcard())
        .collect::<Vec<_>>();
    assert!(
        permits.iter().all(Option::is_some),
        "the paused transfer still holds permits"
    );
    drop(permits);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn resuming_a_running_job_leaves_it_running() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-resume-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/held.bin".into(), vec![3; 4096], true)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(file_request(server.url("/held.bin"), &root.join("out")))
        .unwrap()
        .remove(0);
    wait_until("the download to start", || {
        job_state(&engine, &id).is_active()
    });

    engine.resume(&id).unwrap();

    assert!(job_state(&engine, &id).is_active());
    server.release();
    wait_until("the download to finish", || {
        job_state(&engine, &id) == JobState::Completed
    });
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_running_job_has_received_no_bytes_until_the_server_sends_the_first_one() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-received-{}", Uuid::new_v4()));
    let server = FileServer::start(vec![("/held.bin".into(), vec![3; 4096], true)]);
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let status = || crate::model::AddJobStatus::of(&engine.snapshot().jobs, "extension-add");
    let id = engine
        .add(AddRequest {
            request_id: Some("extension-add".into()),
            ..file_request(server.url("/held.bin"), &root.join("out"))
        })
        .unwrap()
        .remove(0);
    wait_until("the server to be asked for the body", || {
        !server.hits.lock().unwrap().is_empty()
    });
    // The answer's headers are in; the body is held back.
    thread::sleep(Duration::from_millis(600));
    let held = status();
    assert!(held.found);
    assert!(held.state.unwrap().is_active(), "{held:?}");
    assert!(!held.received, "{held:?}");

    server.release();
    wait_until("the download to finish", || {
        job_state(&engine, &id) == JobState::Completed
    });
    let done = status();
    assert_eq!(done.state, Some(JobState::Completed));
    assert!(done.received);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pause_all_cancels_a_running_source_refresh_so_it_cannot_requeue_the_job() {
    let root = std::env::temp_dir().join(format!(
        "ssdownload-engine-refresh-pause-all-{}",
        Uuid::new_v4()
    ));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(refresh_request(
            "https://old.example.test/refresh.bin".into(),
        ))
        .unwrap()
        .remove(0);
    let (state_path, original_state) = seed_direct_refresh_state(&engine, &id);
    let ticket = engine.begin_source_refresh(&id).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let refreshed_url = format!("http://{}/refresh.bin", listener.local_addr().unwrap());
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server =
        thread::spawn(move || read_head_and_respond(listener, Some(started_tx), Some(release_rx)));
    let complete_engine = engine.clone();
    let complete_id = id.clone();
    let complete = thread::spawn(move || {
        let mut request = (*ticket.request).clone();
        request.url = refreshed_url;
        complete_engine.complete_source_refresh(&complete_id, &ticket.token, request, false)
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

    engine.pause_all().unwrap();
    assert_eq!(job_state(&engine, &id), JobState::AwaitingSource);
    release_tx.send(()).unwrap();

    assert!(
        complete.join().unwrap().is_err(),
        "a refresh that finishes after Pause all must be rejected"
    );
    server.join().unwrap();
    assert_eq!(job_state(&engine, &id), JobState::AwaitingSource);
    assert_eq!(fs::read(state_path).unwrap(), original_state);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_job_waiting_for_a_new_source_is_neither_paused_nor_started_by_bulk_commands() {
    let root = std::env::temp_dir().join(format!("ssdownload-engine-awaiting-{}", Uuid::new_v4()));
    let engine = Engine::open(AppPaths::new(root.clone()).unwrap()).unwrap();
    let id = engine
        .add(AddRequest {
            directory: Some(root.join("out")),
            ..refresh_request("https://old.example.test/refresh.bin".into())
        })
        .unwrap()
        .remove(0);
    engine.begin_source_refresh(&id).unwrap();
    assert_eq!(job_state(&engine, &id), JobState::AwaitingSource);

    engine.pause_all().unwrap();
    assert_eq!(job_state(&engine, &id), JobState::AwaitingSource);

    let error = engine.start_now(&id).unwrap_err();
    assert_eq!(error.to_string(), source_refresh_required().to_string());
    assert_eq!(job_state(&engine, &id), JobState::AwaitingSource);
    engine.shutdown();
    fs::remove_dir_all(root).unwrap();
}

mod retry_budget {
    use super::*;
    use crate::test_http::{
        file_sha256, pattern_sha256, repr_digest_header, Config, CutBody, Logged, Scripted, Server,
        Signing, TempDir, Validity,
    };

    const MIB: u64 = 1024 * 1024;

    fn engine_with_retry_limit(root: &Path, retry_limit: u8) -> Engine {
        let engine = Engine::open(AppPaths::new(root.to_path_buf()).unwrap()).unwrap();
        let mut settings = engine.snapshot().settings.clone();
        settings.retry_limit = retry_limit;
        settings.connections = 4;
        settings.per_host_limit = 4;
        engine.update_settings(settings).unwrap();
        engine
    }

    fn add_download(
        engine: &Engine,
        server: &Server,
        directory: &Path,
        checksum: Option<&str>,
    ) -> String {
        engine
            .add(AddRequest {
                checksum: checksum.map(str::to_owned),
                ..file_request(server.url("/payload.bin"), directory)
            })
            .unwrap()
            .remove(0)
    }

    fn finished_job(engine: &Engine, id: &str) -> Job {
        wait_until("the job to end", || job_state(engine, id).is_terminal());
        engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .clone()
    }

    /// Every attempt of a download opens with one HEAD probe.
    fn attempts_made(server: &Server) -> usize {
        server
            .log()
            .iter()
            .filter(|entry| entry.method == "HEAD")
            .count()
    }

    /// The GETs that carry file data, from the `attempt`th attempt (from 1) on.
    fn data_gets_of_attempt(log: &[Logged], attempt: usize) -> Vec<&Logged> {
        let mut heads = 0;
        log.iter()
            .filter(|entry| {
                if entry.method == "HEAD" {
                    heads += 1;
                }
                heads == attempt
                    && entry.method == "GET"
                    && entry.range.as_deref() != Some("bytes=0-0")
            })
            .collect()
    }

    #[test]
    fn a_download_that_drops_more_often_than_the_limit_finishes_while_each_attempt_keeps_bytes() {
        let size = MIB;
        let server = Server::start(Config {
            size,
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: 3,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-progress");
        let engine = engine_with_retry_limit(&root.0, 1);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        // Three attempts end early after keeping bytes, more than the limit of one, and the
        // fourth finishes the file.
        assert_eq!(attempts_made(&server), 4);
        engine.shutdown();
    }

    #[test]
    fn a_limit_of_zero_retries_nothing_even_after_an_attempt_that_kept_bytes() {
        let server = Server::start(Config {
            size: MIB,
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-zero");
        let engine = engine_with_retry_limit(&root.0, 0);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(attempts_made(&server), 1);
        engine.shutdown();
    }

    #[test]
    fn a_source_that_keeps_answering_with_a_server_error_ends_after_the_limit_plus_one_attempts() {
        let server = Server::start(Config {
            size: MIB,
            scripted: vec![
                Scripted {
                    status: 500,
                    retry_after: None,
                };
                50
            ],
            ..Config::default()
        });
        let root = TempDir::new("retry-server-error");
        let engine = engine_with_retry_limit(&root.0, 2);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed);
        // Each attempt sends the HEAD probe and gives up on its answer.
        assert_eq!(server.log().len(), 3);
        engine.shutdown();
    }

    #[test]
    fn a_file_job_redirected_to_a_page_fails_at_once_and_is_neither_retried_nor_sent_to_a_refresh()
    {
        let page = Server::start(Config {
            size: 4_000,
            content_type: Some("text/html".into()),
            ..Config::default()
        });
        let source = Server::start(Config {
            redirect_to: Some(page.url("/signin")),
            ..Config::default()
        });
        let root = TempDir::new("retry-redirected-page");
        let engine = engine_with_retry_limit(&root.0, 3);
        // A job with a page to refresh from goes to the refresh on an authorization failure.
        let id = engine
            .add(AddRequest {
                page_url: Some("https://page.example.test/watch".into()),
                ..file_request(source.url("/report.pdf"), &root.0.join("out"))
            })
            .unwrap()
            .remove(0);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed, "{:?}", job.error);
        let error = job.error.as_deref().unwrap();
        assert!(error.starts_with("SSD-NET-004 "), "{error}");
        assert_eq!(job.attempts, 0);
        let entry = crate::error_codes::definition(crate::error_codes::NET_004).unwrap();
        assert!(!entry.retryable, "the registry must say what the job did");
        assert!(!job.path.exists());
        assert_eq!(attempts_made(&source), 1);
        assert!(page.log().iter().all(|entry| entry.method == "HEAD"));
        engine.shutdown();
    }

    /// What the extension is told, sampled from the add until the job ends.
    fn statuses_until_the_job_ends(
        engine: &Engine,
        request_id: &str,
    ) -> Vec<crate::model::AddJobStatus> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        loop {
            let status = crate::model::AddJobStatus::of(&engine.snapshot().jobs, request_id);
            let ended = status.state.is_some_and(JobState::is_terminal);
            seen.push(status);
            if ended {
                return seen;
            }
            assert!(Instant::now() < deadline, "the job did not end");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A job that the page check refuses reports no received bytes at any moment, however far
    /// the transfer got before the page came: the probe sees the page, the single-connection
    /// transfer is redirected to it after the probe saw a file, and every range of a
    /// multi-connection transfer is answered with it. The pages that a transfer could read are
    /// slow, so a page that were written would be counted in progress while the job still runs.
    #[test]
    fn a_job_refused_by_the_page_check_never_reports_received_bytes() {
        const PAGE: u64 = 400_000;
        let page = |throttle| {
            Server::start(Config {
                size: PAGE,
                content_type: Some("text/html".into()),
                throttle: Some(throttle),
                ..Config::default()
            })
        };
        let ranged_page = Server::start(Config {
            size: 6 * MIB,
            content_type: Some("application/octet-stream".into()),
            content_type_from: Some((2, "text/html".into())),
            throttle: Some(MIB),
            ..Config::default()
        });
        let probe_sees_page = page(100_000);
        let get_sees_page = page(100_000);
        let sources = [
            (
                "the probe sees the page",
                Server::start(Config {
                    redirect_to: Some(probe_sees_page.url("/signin")),
                    ..Config::default()
                }),
                &probe_sees_page,
            ),
            (
                "the transfer is redirected to the page",
                Server::start(Config {
                    size: PAGE,
                    content_type: Some("application/octet-stream".into()),
                    redirect_to: Some(get_sees_page.url("/signin")),
                    head_not_redirected: true,
                    ..Config::default()
                }),
                &get_sees_page,
            ),
            (
                "every range is answered with the page",
                Server::start(Config {
                    redirect_to: Some(ranged_page.url("/signin")),
                    ..Config::default()
                }),
                &ranged_page,
            ),
        ];
        let root = TempDir::new("add-status-page");
        let engine = engine_with_retry_limit(&root.0, 3);
        for (index, (label, source, page)) in sources.iter().enumerate() {
            let request_id = format!("takeover-{index}");
            engine
                .add(AddRequest {
                    request_id: Some(request_id.clone()),
                    ..file_request(source.url("/report.pdf"), &root.0.join("out"))
                })
                .unwrap();
            let seen = statuses_until_the_job_ends(&engine, &request_id);
            assert!(
                seen.iter().all(|status| status.found && !status.received),
                "{label}: {seen:?}"
            );
            let last = seen.last().unwrap();
            assert_eq!(last.state, Some(JobState::Failed), "{label}: {last:?}");
            assert_eq!(last.error_code.as_deref(), Some("SSD-NET-004"), "{label}");
            let job = engine
                .snapshot()
                .jobs
                .iter()
                .find(|job| job.request.request_id.as_deref() == Some(request_id.as_str()))
                .unwrap()
                .clone();
            assert_eq!(job.downloaded, 0, "{label}");
            assert!(!job.path.exists(), "{label}: the page was published");
            if index > 0 {
                page.wait_idle();
                assert!(
                    page.log().iter().any(|entry| entry.method == "GET"
                        && entry.range.as_deref() != Some("bytes=0-0")),
                    "{label}: the transfer should have reached the page: {:?}",
                    page.log()
                );
            }
        }
        engine.shutdown();
    }

    #[test]
    fn a_server_that_cuts_every_range_still_finishes_because_each_cut_keeps_its_bytes() {
        // Every response longer than 1 MiB is cut after 1 MiB. A lease that is cut resumes
        // where it stopped, so each cut makes progress and the limit of one is never used up.
        let size = 12 * MIB;
        let server = Server::start(Config {
            size,
            cut_body: Some(CutBody {
                after_bytes: MIB,
                responses: usize::MAX,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-cut-keeps-bytes");
        let engine = engine_with_retry_limit(&root.0, 1);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        // Nothing a cut response delivered is fetched again.
        assert!(
            server.body_bytes_sent() < size + MIB,
            "{} body bytes for a {size} byte file",
            server.body_bytes_sent()
        );
        engine.shutdown();
    }

    #[test]
    fn a_source_whose_identity_changes_every_other_attempt_ends_after_the_limit() {
        // The tag changes after every second attempt. The attempt after a change restarts and
        // keeps one cut's worth, the next one resumes and regrows to what the job had retained
        // before the change: more than that attempt started with, but never more than the job
        // has retained at any time.
        let server = Server::start(Config {
            size: MIB,
            etag_rotation_heads: Some(2),
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: usize::MAX,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-flipping-identity");
        let engine = engine_with_retry_limit(&root.0, 2);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(attempts_made(&server), 4);
        engine.shutdown();
    }

    #[test]
    fn an_attempt_that_restarted_is_no_progress_although_it_retains_more_than_the_job_ever_had() {
        // The tag changes with every attempt, so each one restarts from the beginning, and each
        // cut lets more bytes through than the one before: the retained amount grows past the
        // earlier high mark, yet the restarts keep counting against the limit.
        let server = Server::start(Config {
            size: MIB,
            etag_rotation_heads: Some(1),
            cut_body: Some(CutBody {
                after_bytes: 100 * 1024,
                responses: usize::MAX,
            }),
            cut_growth: 100 * 1024,
            ..Config::default()
        });
        let root = TempDir::new("retry-restart-regrows");
        let engine = engine_with_retry_limit(&root.0, 1);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed, "{:?}", job.error);
        assert_eq!(attempts_made(&server), 2);
        engine.shutdown();
    }

    #[test]
    fn a_refreshed_source_with_a_restart_is_judged_afresh_not_against_the_old_high_mark() {
        // The old source keeps 600 KiB before the job is paused. The refreshed source restarts
        // the download and is cut every 300 KiB: each attempt retains more than the one before,
        // although never more than the old source had.
        let old = Server::start(Config {
            size: MIB,
            cut_body: Some(CutBody {
                after_bytes: 600 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let new = Server::start(Config {
            size: MIB,
            cut_body: Some(CutBody {
                after_bytes: 300 * 1024,
                responses: usize::MAX,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-refresh");
        let engine = engine_with_retry_limit(&root.0, 1);
        let id = engine
            .add(AddRequest {
                filename: Some("refresh.bin".into()),
                page_url: Some("https://example.test/watch/refresh".into()),
                source_identity: Some(SourceIdentity {
                    video_id: "refresh-video".into(),
                    frame_id: 7,
                    document_id: None,
                    page_url: "https://example.test/watch/refresh".into(),
                }),
                ..file_request(old.url("/payload.bin"), &root.0.join("out"))
            })
            .unwrap()
            .remove(0);
        wait_until("the cut response", || {
            old.log().iter().any(|entry| entry.body_cut)
        });
        old.wait_idle();
        engine.pause(&id).unwrap();
        wait_until("the pause", || job_state(&engine, &id) == JobState::Paused);

        // Paused is reported before the download thread has stopped, which refuses the
        // refresh without changing anything.
        let deadline = Instant::now() + Duration::from_secs(10);
        let ticket = loop {
            match engine.begin_source_refresh(&id) {
                Ok(ticket) => break ticket,
                Err(error) => {
                    assert!(Instant::now() < deadline, "{error:#}");
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let mut request = (*ticket.request).clone();
        request.url = new.url("/payload.bin");
        engine
            .complete_source_refresh(&id, &ticket.token, request, true)
            .unwrap();

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, MIB));
        assert_eq!(attempts_made(&new), 4);
        engine.shutdown();
    }

    #[test]
    fn a_checksum_that_never_matches_ends_the_download_although_the_attempts_moved_bytes() {
        let size = MIB;
        let server = Server::start(Config {
            size,
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-checksum");
        let limit = 1u8;
        let engine = engine_with_retry_limit(&root.0, limit);
        let id = add_download(&engine, &server, &root.0.join("out"), Some(&"0".repeat(64)));

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed);
        assert!(
            job.error
                .as_deref()
                .unwrap_or_default()
                .contains("SSD-TRF-003"),
            "{:?}",
            job.error
        );
        // The first attempt keeps bytes and does not count; every attempt that downloads the
        // file and fails the digest does, so at most the limit plus one of them are made.
        assert!(attempts_made(&server) <= 1 + usize::from(limit) + 1);
        assert!(!job.path.exists());
        let left = fs::read_dir(job.work_dir.as_ref().unwrap())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.contains(".ssdownload."))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(left.is_empty(), "partial files left behind: {left:?}");
        engine.shutdown();
    }

    #[test]
    fn a_retry_that_reports_less_than_the_attempt_before_it_counts_only_the_bytes_beyond_it() {
        // The reports are those of the app's HLS downloader: a failed attempt has reported its
        // stored segments and the part of a segment still coming, and the next attempt starts
        // from the stored segments. The server holds every answer back for two seconds, so each
        // attempt waits while its reports go in, and answers the first attempt's probe with 500.
        let server = Server::start(Config {
            size: 64 * 1024,
            scripted: vec![Scripted {
                status: 500,
                retry_after: None,
            }],
            latency: Some(Duration::from_secs(2)),
            ..Config::default()
        });
        let root = TempDir::new("retry-quota-once");
        let engine = engine_with_retry_limit(&root.0, 3);
        let queue = give_the_default_queue_a_quota(&engine);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        wait_until("the first attempt", || attempts_made(&server) == 1);
        report_downloaded(&engine, &id, &[400_000, 1_100_000]);
        wait_until("the retry", || attempts_made(&server) == 2);
        report_downloaded(&engine, &id, &[1_000_000, 1_200_000, 1_500_000]);
        let path = engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .path
            .clone();
        send_to_the_worker(&engine, &|generation| WorkerEvent::Finished {
            id: id.clone(),
            generation,
            result: Ok(TransferResult {
                path: path.clone(),
                bytes: 1_500_000,
                warning: None,
            }),
        });
        wait_until("the job", || job_state(&engine, &id) == JobState::Completed);

        assert_eq!(consumed_quota_bytes(&engine, &queue), 1_500_000);
        engine.shutdown();
    }

    #[test]
    fn retrying_a_download_that_failed_and_kept_nothing_counts_its_bytes_again() {
        // A SHA-256 that does not match deletes the file, so each retry, which only the user
        // starts, downloads it again from the beginning.
        let size = MIB;
        let server = Server::start(Config {
            size,
            ..Config::default()
        });
        let root = TempDir::new("retry-quota-again");
        let engine = engine_with_retry_limit(&root.0, 1);
        let queue = give_the_default_queue_a_quota(&engine);
        let id = add_download(&engine, &server, &root.0.join("out"), Some(&"0".repeat(64)));
        assert_eq!(finished_job(&engine, &id).state, JobState::Failed);
        assert_eq!(consumed_quota_bytes(&engine, &queue), size);

        engine.retry_failed().unwrap();
        assert_eq!(finished_job(&engine, &id).state, JobState::Failed);
        assert_eq!(consumed_quota_bytes(&engine, &queue), 2 * size);

        // So does a retry after the app was closed in between.
        engine.shutdown();
        let engine = Engine::open(AppPaths::new(root.0.clone()).unwrap()).unwrap();
        engine.retry_failed().unwrap();
        assert_eq!(finished_job(&engine, &id).state, JobState::Failed);
        assert_eq!(consumed_quota_bytes(&engine, &queue), 3 * size);
        assert_eq!(server.body_bytes_sent(), 3 * size);
        engine.shutdown();
    }

    #[test]
    fn retrying_a_download_that_failed_and_kept_bytes_does_not_count_them_again() {
        // Slow enough for the attempt to report bytes before its connection is cut.
        let size = MIB;
        let server = Server::start(Config {
            size,
            throttle: Some(512 * 1024),
            cut_body: Some(CutBody {
                after_bytes: 300 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-quota-kept");
        let engine = engine_with_retry_limit(&root.0, 0);
        let queue = give_the_default_queue_a_quota(&engine);
        let id = add_download(&engine, &server, &root.0.join("out"), None);
        assert_eq!(finished_job(&engine, &id).state, JobState::Failed);
        assert!(consumed_quota_bytes(&engine, &queue) > 0);

        engine.retry_failed().unwrap();
        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(consumed_quota_bytes(&engine, &queue), size);
        engine.shutdown();
    }

    #[test]
    fn a_source_refresh_that_goes_on_counts_only_the_bytes_beyond_the_most_the_job_counted() {
        // The attempt's last report is below the most it reported, as when a downloader that
        // started again reports the bytes it kept, and then its address has expired. The server
        // holds its answers back for two seconds, so the reports go in while the attempt waits,
        // and refuses the probe and the GET it falls back to.
        let refused = Scripted {
            status: 403,
            retry_after: None,
        };
        let old = Server::start(Config {
            size: MIB,
            scripted: vec![refused.clone(), refused],
            latency: Some(Duration::from_secs(2)),
            ..Config::default()
        });
        let new = Server::start(Config {
            size: MIB,
            ..Config::default()
        });
        let root = TempDir::new("refresh-quota");
        let engine = engine_with_retry_limit(&root.0, 0);
        let queue = give_the_default_queue_a_quota(&engine);
        let id = add_download_from_page(&engine, &old, &root.0.join("out"));
        wait_until("the first request", || attempts_made(&old) == 1);
        report_downloaded(&engine, &id, &[600_000, 400_000]);
        wait_until("the job to wait for a new source", || {
            job_state(&engine, &id) == JobState::AwaitingSource
        });

        let ticket = engine.begin_source_refresh(&id).unwrap();
        let mut request = (*ticket.request).clone();
        request.url = new.url("/payload.bin");
        engine
            .complete_source_refresh(&id, &ticket.token, request, false)
            .unwrap();
        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);

        assert_eq!(consumed_quota_bytes(&engine, &queue), MIB);
        engine.shutdown();
    }

    /// A source whose `/file` redirects to a signed address that lives as `first` says for the
    /// first request and, for later ones, as `later` says; expired addresses answer 403.
    fn signed_source(size: u64, first: Validity, later: Validity) -> Server {
        Server::start(Config {
            size,
            throttle: Some(16 * MIB),
            signing: Some(Signing {
                from: "/file".into(),
                origin: String::new(),
                validity: vec![first, later],
            }),
            expiry: Some(403),
            ..Config::default()
        })
    }

    fn asks_for_the_source(server: &Server) -> usize {
        server
            .log()
            .iter()
            .filter(|entry| entry.path == "/file")
            .count()
    }

    #[test]
    fn a_redirect_target_that_expires_midway_is_followed_again_and_costs_no_retry() {
        // Five ranges for four connections: the fifth is requested after the first address
        // has expired. With a limit of 0 nothing is retried, so the download only finishes if
        // asking the source again is not an attempt. The first address answers its first six
        // requests (the two that probe the file and the first four ranges), however long they
        // take to arrive, and the next one, the fifth range, finds it expired; the address the
        // source gives then never expires, so asking for it once is enough.
        let size = 36 * MIB;
        let server = signed_source(
            size,
            Validity {
                lifetime: Duration::from_secs(3600),
                requests: 6,
            },
            Validity {
                lifetime: Duration::from_secs(3600),
                requests: usize::MAX,
            },
        );
        let root = TempDir::new("retry-expiry-followed");
        let engine = engine_with_retry_limit(&root.0, 0);
        let id = engine
            .add(file_request(server.url("/file"), &root.0.join("out")))
            .unwrap()
            .remove(0);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        assert_eq!(job.attempts, 0);
        assert_eq!(asks_for_the_source(&server), 2);
        engine.shutdown();
    }

    #[test]
    fn a_fresh_target_that_is_refused_too_sends_the_job_to_a_new_source_without_retrying_it() {
        let size = 36 * MIB;
        let server = signed_source(
            size,
            Validity {
                lifetime: Duration::from_millis(250),
                requests: usize::MAX,
            },
            Validity {
                lifetime: Duration::ZERO,
                requests: usize::MAX,
            },
        );
        let root = TempDir::new("retry-expiry-refused");
        let engine = engine_with_retry_limit(&root.0, 3);
        let id = engine
            .add(AddRequest {
                page_url: Some("https://page.example.test/watch".into()),
                ..file_request(server.url("/file"), &root.0.join("out"))
            })
            .unwrap()
            .remove(0);

        wait_until("the job to wait for a new source", || {
            job_state(&engine, &id) == JobState::AwaitingSource
        });
        server.wait_idle();
        let job = engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .clone();
        assert_eq!(job.attempts, 0);
        assert_eq!(asks_for_the_source(&server), 2);
        let kept = fs::read_dir(job.work_dir.as_ref().unwrap())
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".ssdownload."));
        assert!(kept, "the part file was not kept");
        engine.shutdown();
    }

    /// A request for `payload.bin` that carries a page address, which would let a failure that
    /// looks like an expired link ask the page for a new source.
    fn add_download_from_page(engine: &Engine, server: &Server, directory: &Path) -> String {
        engine
            .add(AddRequest {
                page_url: Some("https://example.test/watch/digest".into()),
                ..file_request(server.url("/payload.bin"), directory)
            })
            .unwrap()
            .remove(0)
    }

    /// What the job's work folder keeps of the download: its part, resume record and the record
    /// of a used re-download.
    fn kept_in(job: &Job) -> Vec<String> {
        fs::read_dir(job.work_dir.as_ref().unwrap())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.contains(".ssdownload."))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    fn gets(server: &Server) -> usize {
        server
            .log()
            .iter()
            .filter(|entry| entry.method == "GET")
            .count()
    }

    #[test]
    fn two_downloads_that_differ_and_miss_the_published_digest_end_after_two_downloads_whatever_the_limit(
    ) {
        let size = MIB;
        // The digest is that of body version 3. The first pass gets version 1 and the second one
        // version 2: no pass meets the digest and the passes differ from each other.
        let server = Server::start(Config {
            size,
            extra_headers: vec![repr_digest_header(3, size)],
            switch_version_at_head: Some(2),
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-published-digest");
        let engine = engine_with_retry_limit(&root.0, 20);
        let id = add_download_from_page(&engine, &server, &root.0.join("out"));

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed, "{:?}", job.error);
        assert!(
            job.error
                .as_deref()
                .unwrap_or_default()
                .contains("SSD-TRF-003"),
            "{:?}",
            job.error
        );
        // The first attempt is cut after keeping bytes and the second finishes the file, finds
        // the digest wrong and fetches the file once more within itself: three probes in all,
        // although the limit of twenty would allow many more. The file is read in full twice.
        let log = server.log();
        assert_eq!(attempts_made(&server), 3, "{log:?}");
        assert_eq!(gets(&server), 3, "{log:?}");
        assert!(server.body_bytes_sent() >= 2 * size);
        assert!(server.body_bytes_sent() < 3 * size);
        assert!(!job.path.exists());
        assert_eq!(kept_in(&job), Vec::<String>::new());
        engine.shutdown();
    }

    #[test]
    fn a_stale_published_digest_does_not_fail_a_download_that_two_passes_agree_on() {
        let size = MIB;
        // The server always serves body version 1 and publishes the digest of version 2.
        let server = Server::start(Config {
            size,
            extra_headers: vec![repr_digest_header(2, size)],
            cut_body: Some(CutBody {
                after_bytes: 150 * 1024,
                responses: 1,
            }),
            ..Config::default()
        });
        let root = TempDir::new("retry-stale-digest");
        let engine = engine_with_retry_limit(&root.0, 20);
        let id = add_download_from_page(&engine, &server, &root.0.join("out"));

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        // The note on the job names the source of the digest.
        let note = job.error.as_deref().unwrap_or_default();
        assert!(note.contains("Repr-Digest"), "{note}");
        assert_eq!(attempts_made(&server), 3);
        assert_eq!(gets(&server), 3);
        assert!(server.body_bytes_sent() < 3 * size);
        assert_eq!(kept_in(&job), Vec::<String>::new());
        engine.shutdown();
    }

    /// A job whose first pass met a digest it did not match and whose second pass was refused:
    /// the server answers the second `HEAD` with 404. `differ` makes the next pass serve other
    /// bytes than the first; otherwise the bytes stay the same and only the digest is stale.
    fn failed_in_its_second_pass(root: &TempDir, differ: bool) -> (Engine, Server, Job) {
        let size = MIB;
        let server = Server::start(Config {
            size,
            extra_headers: vec![repr_digest_header(3, size)],
            switch_version_at_head: differ.then_some(2),
            head_statuses: vec![None, Some(404)],
            ..Config::default()
        });
        let engine = engine_with_retry_limit(&root.0, 20);
        let id = add_download(&engine, &server, &root.0.join("out"), None);
        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Failed, "{:?}", job.error);
        assert!(
            !job.error
                .as_deref()
                .unwrap_or_default()
                .contains("SSD-TRF-003"),
            "{:?}",
            job.error
        );
        // The re-download was granted and recorded before the first bytes were discarded, and
        // the failed pass left nothing else.
        let kept = kept_in(&job);
        assert!(
            kept.iter().all(|name| name.ends_with(".digest-retry")) && kept.len() == 1,
            "{kept:?}"
        );
        assert_eq!(server.body_bytes_sent(), size);
        (engine, server, job)
    }

    #[test]
    fn a_second_pass_that_fails_for_another_reason_keeps_the_one_re_download_for_a_retry() {
        let size = MIB;
        // Stale digest, same bytes: the retry is the job's second download and ends the matter.
        let root = TempDir::new("retry-second-pass-refused-stale");
        let (engine, server, job) = failed_in_its_second_pass(&root, false);
        engine.retry_failed().unwrap();
        let job = finished_job(&engine, &job.id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        assert!(job
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Repr-Digest"));
        // Two downloads of the file in all, not three, and no record is left behind.
        assert_eq!(server.body_bytes_sent(), 2 * size);
        assert_eq!(kept_in(&job), Vec::<String>::new());
        engine.shutdown();

        // Other bytes: the retry is the second download as well, and fails the job for good.
        let root = TempDir::new("retry-second-pass-refused-differ");
        let (engine, server, job) = failed_in_its_second_pass(&root, true);
        engine.retry_failed().unwrap();
        let job = finished_job(&engine, &job.id);
        assert_eq!(job.state, JobState::Failed, "{:?}", job.error);
        assert!(job
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("SSD-TRF-003"));
        assert_eq!(server.body_bytes_sent(), 2 * size);
        assert_eq!(kept_in(&job), Vec::<String>::new());
        engine.shutdown();
    }

    #[test]
    fn removing_a_job_that_failed_in_its_second_pass_removes_the_record_of_the_used_re_download() {
        let root = TempDir::new("retry-second-pass-removed");
        let (engine, _server, job) = failed_in_its_second_pass(&root, false);
        let work = job.work_dir.clone().unwrap();
        assert!(work.exists());
        engine.remove(&job.id, false).unwrap();
        wait_until("the work folder to go", || !work.exists());
        engine.shutdown();
    }

    #[test]
    fn an_attempt_cut_short_by_closing_the_app_is_not_counted() {
        let root = TempDir::new("retry-shutdown");
        let server = FileServer::start(vec![("/slow.bin".into(), vec![1; 4096], true)]);
        let engine = Engine::open(AppPaths::new(root.0.clone()).unwrap()).unwrap();
        let id = engine
            .add(file_request(server.url("/slow.bin"), &root.0.join("out")))
            .unwrap()
            .remove(0);
        wait_until("the request to reach the server", || {
            !server.hits.lock().unwrap().is_empty()
        });
        engine.shutdown();

        server.release();
        let engine = Engine::open(AppPaths::new(root.0.clone()).unwrap()).unwrap();
        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(job.attempts, 0);
        engine.shutdown();
    }

    #[test]
    fn a_progressing_attempt_without_throttling_gives_the_connections_back_to_the_next_attempt() {
        // Four connections are configured. The HEAD of the first attempt is answered 500, which
        // halves the cap to two. The second attempt downloads the first half and loses the
        // second one three times: it kept bytes and met no 429 or 503, so the cap is restored
        // and the third attempt splits what is left over four connections instead of two.
        let size = 16 * MIB;
        let server = Server::start(Config {
            size,
            scripted: vec![Scripted {
                status: 500,
                retry_after: None,
            }],
            cut_body: Some(CutBody {
                after_bytes: 256 * 1024,
                responses: 3,
            }),
            cut_from: 8 * MIB,
            ..Config::default()
        });
        let root = TempDir::new("retry-cap-restored");
        let engine = engine_with_retry_limit(&root.0, 3);
        let id = add_download(&engine, &server, &root.0.join("out"), None);

        let job = finished_job(&engine, &id);
        assert_eq!(job.state, JobState::Completed, "{:?}", job.error);
        assert_eq!(file_sha256(&job.path), pattern_sha256(1, size));
        let log = server.log();
        assert_eq!(attempts_made(&server), 3);
        // With two connections the rest of the second half is one lease; with four it is split
        // in two.
        assert_eq!(data_gets_of_attempt(&log, 3).len(), 2);
        engine.shutdown();
    }

    #[test]
    fn the_cap_rule_halves_without_progress_and_restores_the_current_cap_one_step() {
        let configured = 8;
        let mut cap = retry_connection_cap_after(None, 8, configured, false, false);
        assert_eq!(cap, Some(4));
        cap = retry_connection_cap_after(cap, 4, configured, false, false);
        assert_eq!(cap, Some(2));
        // Progress that met a 429 or 503 is no reason to give connections back.
        assert_eq!(
            retry_connection_cap_after(cap, 2, configured, true, true),
            Some(1)
        );
        // The step is taken from the cap, not from the connections the attempt happened to use.
        assert_eq!(
            retry_connection_cap_after(cap, 1, configured, true, false),
            Some(4)
        );
        cap = retry_connection_cap_after(cap, 2, configured, true, false);
        assert_eq!(cap, Some(4));
        assert_eq!(
            retry_connection_cap_after(cap, 4, configured, true, false),
            None
        );
        // An attempt at the full count that progressed leaves no cap behind.
        assert_eq!(
            retry_connection_cap_after(None, 8, configured, true, false),
            None
        );
    }

    #[test]
    fn a_failure_is_throttled_when_the_error_or_the_transfer_saw_429_or_503() {
        let request = file_request("https://files.example.test/a.bin".into(), Path::new("out"));
        let control = TransferControl::default();
        let plain = |message: &str| attempt_failure(&request, anyhow!("{message}"), &control);
        assert!(!plain("connection reset").throttled);
        assert!(plain("HTTP Error 429: Too Many Requests").throttled);
        assert!(plain("HTTP Error 503: Service Unavailable").throttled);
        assert!(!plain("HTTP Error 500: Server Error").throttled);

        control.mark_throttled();
        assert!(plain("connection reset").throttled);
    }

    #[test]
    fn an_attempt_progresses_only_above_the_most_the_job_ever_retained() {
        let mut high = HashMap::new();
        let ended = |before, after| {
            let control = TransferControl::default();
            control.set_retained(before, after);
            control
        };
        // An amount that could not be measured is no progress.
        assert!(!attempt_progressed(
            &mut high,
            "job",
            &TransferControl::default()
        ));
        assert!(attempt_progressed(&mut high, "job", &ended(0, 100)));
        // Regrowing to what the job already had retained is not progress.
        assert!(!attempt_progressed(&mut high, "job", &ended(40, 100)));
        assert!(attempt_progressed(&mut high, "job", &ended(100, 101)));
        // A restart never is, but what it retained still raises the mark.
        let restarted = ended(101, 500);
        restarted.mark_restarted();
        assert!(!attempt_progressed(&mut high, "job", &restarted));
        assert!(!attempt_progressed(&mut high, "job", &ended(101, 400)));
        // A live recording never is.
        let live = ended(500, 900);
        live.mark_live();
        assert!(!attempt_progressed(&mut high, "job", &live));
        // Other jobs have their own mark.
        assert!(attempt_progressed(&mut high, "other", &ended(0, 5)));
    }
}

mod holds {
    use super::*;

    const POLL: Duration = Duration::from_millis(20);
    /// Several scheduler passes (one every 150 ms).
    const SETTLE: Duration = Duration::from_millis(600);

    fn open(root: &Path, sensors: &FakeSensors) -> Engine {
        let engine = Engine::open_with(
            AppPaths::new(root.to_path_buf()).unwrap(),
            Box::new(sensors.clone()),
            POLL,
        )
        .unwrap();
        let mut settings = engine.snapshot().settings;
        settings.pause_on_metered = true;
        settings.pause_on_battery_saver = true;
        engine.update_settings(settings).unwrap();
        engine
    }

    fn sensor(sensors: &FakeSensors, battery_saver: bool) -> &AtomicBool {
        if battery_saver {
            &sensors.battery_saver
        } else {
            &sensors.metered
        }
    }

    fn switch(sensors: &FakeSensors, battery_saver: bool, on: bool) {
        sensor(sensors, battery_saver).store(on, Ordering::Release);
    }

    fn wait_for_hold(engine: &Engine, active: bool) {
        wait_until("the hold to change", || {
            engine.snapshot().hold.is_active() == active
        });
    }

    fn add(engine: &Engine, server: &FileServer, path: &str, directory: &Path) -> String {
        engine
            .add(file_request(server.url(path), directory))
            .unwrap()
            .remove(0)
    }

    fn requests(server: &FileServer, path: &str) -> usize {
        server
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| *hit == path)
            .count()
    }

    fn job(engine: &Engine, id: &str) -> Job {
        engine
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .clone()
    }

    #[test]
    fn a_running_job_goes_back_to_the_queue_when_a_hold_starts_and_runs_again_when_it_lifts() {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![("/held.bin".into(), vec![5; 4096], true)]);
            let sensors = FakeSensors::default();
            let engine = open(&root, &sensors);
            let id = add(&engine, &server, "/held.bin", &root.join("out"));
            wait_until("the download to start", || {
                job_state(&engine, &id).is_active()
            });

            switch(&sensors, battery_saver, true);
            wait_until("the hold to send the job back to the queue", || {
                job_state(&engine, &id) == JobState::Queued
            });
            let hold = engine.snapshot().hold;
            assert_eq!(
                (hold.metered, hold.battery_saver),
                (!battery_saver, battery_saver)
            );
            // The row names the hold from the moment the hold sends it back, while the worker that was
            // stopped is still ending.
            let notice = hold.notice().unwrap();
            assert_eq!(job(&engine, &id).phase, notice);
            let started = requests(&server, "/held.bin");
            thread::sleep(SETTLE);
            assert_eq!(job_state(&engine, &id), JobState::Queued);
            assert_eq!(requests(&server, "/held.bin"), started);
            assert_eq!(job(&engine, &id).phase, notice);

            switch(&sensors, battery_saver, false);
            wait_until("the job to run again", || {
                job_state(&engine, &id).is_active()
            });
            assert!(!engine.snapshot().hold.is_active());
            server.release();
            wait_until("the download to finish", || {
                job_state(&engine, &id) == JobState::Completed
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_job_the_hold_stops_names_the_hold_in_every_snapshot_that_shows_it_queued() {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![("/held.bin".into(), vec![5; 4096], true)]);
            let sensors = FakeSensors::default();
            let engine = open(&root, &sensors);
            let id = add(&engine, &server, "/held.bin", &root.join("out"));
            wait_until("the download to start", || {
                job_state(&engine, &id).is_active()
            });

            switch(&sensors, battery_saver, true);
            // The stopped worker takes a moment to end, and the job is Queued from the stop on: the
            // snapshots are read every millisecond so that the first one that shows it Queued is read.
            let mut queued = Vec::new();
            let mut first = None;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline
                && first.is_none_or(|first: Instant| first.elapsed() < SETTLE)
            {
                let snapshot = engine.snapshot();
                let row = snapshot.jobs.iter().find(|job| job.id == id).unwrap();
                if row.state == JobState::Queued {
                    first.get_or_insert_with(Instant::now);
                    queued.push((row.phase.clone(), snapshot.hold.notice()));
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(!queued.is_empty(), "the hold never sent the job back");
            for (phase, notice) in &queued {
                assert_eq!(Some(phase.as_str()), *notice, "a Queued snapshot");
            }

            switch(&sensors, battery_saver, false);
            wait_until("the job to run again", || {
                job_state(&engine, &id).is_active()
            });
            server.release();
            wait_until("the download to finish", || {
                job_state(&engine, &id) == JobState::Completed
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_job_the_hold_does_not_stop_never_shows_the_hold_text() {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![
                ("/live.bin".into(), vec![1; 4096], true),
                ("/now.bin".into(), vec![2; 4096], true),
                ("/later.bin".into(), vec![3; 4096], true),
            ]);
            let sensors = FakeSensors::default();
            switch(&sensors, battery_saver, true);
            let engine = open(&root, &sensors);
            wait_for_hold(&engine, true);
            let live = add(&engine, &server, "/live.bin", &root.join("out"));
            mark_as_live_recording(&live);
            switch(&sensors, battery_saver, false);
            wait_until("the live recording to start", || {
                job_state(&engine, &live).is_active()
            });

            // A running live recording and a job that Start now forced run through the hold; a job
            // added during it waits.
            switch(&sensors, battery_saver, true);
            wait_for_hold(&engine, true);
            let notice = engine.snapshot().hold.notice().unwrap();
            let now = add(&engine, &server, "/now.bin", &root.join("out"));
            let later = add(&engine, &server, "/later.bin", &root.join("out"));
            engine.start_now(&now).unwrap();
            let until = Instant::now() + SETTLE;
            while Instant::now() < until {
                let snapshot = engine.snapshot();
                for (name, id) in [("live", &live), ("start now", &now), ("later", &later)] {
                    let row = snapshot.jobs.iter().find(|job| &job.id == id).unwrap();
                    if name == "later" {
                        assert_eq!((row.state, row.phase.as_str()), (JobState::Queued, notice));
                    } else {
                        assert_ne!(row.phase, notice, "the {name} job is not held");
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(job_state(&engine, &live).is_active());
            assert!(job_state(&engine, &now).is_active());

            switch(&sensors, battery_saver, false);
            wait_until("the waiting job to start", || {
                job_state(&engine, &later).is_active()
            });
            server.release();
            wait_until("every download to finish", || {
                [&live, &now, &later]
                    .iter()
                    .all(|id| job_state(&engine, id) == JobState::Completed)
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_row_that_cannot_start_when_a_hold_lifts_shows_the_ordinary_queue_text_also_after_a_reopen()
    {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![
                ("/a.bin".into(), vec![1; 4096], true),
                ("/b.bin".into(), vec![2; 4096], true),
            ]);
            let sensors = FakeSensors::default();
            let engine = open(&root, &sensors);
            let a = add(&engine, &server, "/a.bin", &root.join("out"));
            let b = add(&engine, &server, "/b.bin", &root.join("out"));
            wait_until("both downloads to start", || {
                job_state(&engine, &a).is_active() && job_state(&engine, &b).is_active()
            });

            switch(&sensors, battery_saver, true);
            wait_for_hold(&engine, true);
            let held_text = engine.snapshot().hold.notice().unwrap();
            wait_until("both rows to name the hold", || {
                [&a, &b]
                    .iter()
                    .all(|id| job(&engine, id).phase == held_text)
            });
            thread::sleep(SETTLE);

            let mut settings = engine.snapshot().settings;
            settings.max_active = 1;
            engine.update_settings(settings).unwrap();
            switch(&sensors, battery_saver, false);
            wait_until("one download to start", || {
                [&a, &b].iter().any(|id| job_state(&engine, id).is_active())
            });
            thread::sleep(SETTLE);
            let waiting = [&a, &b]
                .into_iter()
                .filter(|id| !job_state(&engine, id).is_active())
                .collect::<Vec<_>>();
            assert_eq!(waiting.len(), 1);
            assert_eq!(job(&engine, waiting[0]).phase, JobState::Queued.label());
            engine.shutdown();

            let paths = AppPaths::new(root.clone()).unwrap();
            let stored = Store::open(&paths.database).unwrap().load_jobs().unwrap();
            assert_eq!(stored.len(), 2);
            assert!(stored.iter().all(|job| job.phase != held_text));

            let engine = open(&root, &sensors);
            wait_until("one download to start after the reopen", || {
                [&a, &b].iter().any(|id| job_state(&engine, id).is_active())
            });
            thread::sleep(SETTLE);
            let waiting = [&a, &b]
                .into_iter()
                .filter(|id| !job_state(&engine, id).is_active())
                .collect::<Vec<_>>();
            assert_eq!(waiting.len(), 1);
            assert_eq!(job(&engine, waiting[0]).phase, JobState::Queued.label());

            server.release();
            wait_until("both downloads to finish", || {
                [&a, &b]
                    .iter()
                    .all(|id| job_state(&engine, id) == JobState::Completed)
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_job_the_user_paused_stays_paused_through_a_hold_and_its_release() {
        let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
        let server = FileServer::start(vec![
            ("/paused.bin".into(), vec![1; 4096], true),
            ("/running.bin".into(), vec![2; 4096], true),
        ]);
        let sensors = FakeSensors::default();
        let engine = open(&root, &sensors);
        let paused = add(&engine, &server, "/paused.bin", &root.join("out"));
        let running = add(&engine, &server, "/running.bin", &root.join("out"));
        wait_until("both downloads to start", || {
            job_state(&engine, &paused).is_active() && job_state(&engine, &running).is_active()
        });
        engine.pause(&paused).unwrap();
        thread::sleep(SETTLE);
        let paused_requests = requests(&server, "/paused.bin");

        switch(&sensors, false, true);
        wait_until("the hold to send the running job back", || {
            job_state(&engine, &running) == JobState::Queued
        });
        assert_eq!(job_state(&engine, &paused), JobState::Paused);
        assert_eq!(job(&engine, &paused).phase, JobState::Paused.label());

        switch(&sensors, false, false);
        wait_until("the running job to run again", || {
            job_state(&engine, &running).is_active()
        });
        server.release();
        wait_until("the running job to finish", || {
            job_state(&engine, &running) == JobState::Completed
        });
        thread::sleep(SETTLE);
        assert_eq!(job_state(&engine, &paused), JobState::Paused);
        assert_eq!(requests(&server, "/paused.bin"), paused_requests);
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nothing_starts_during_a_hold_and_the_queue_runs_when_it_lifts() {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![
                ("/a.bin".into(), vec![1; 4096], true),
                ("/b.bin".into(), vec![2; 4096], true),
            ]);
            let sensors = FakeSensors::default();
            switch(&sensors, battery_saver, true);
            let engine = open(&root, &sensors);
            wait_for_hold(&engine, true);
            let a = add(&engine, &server, "/a.bin", &root.join("out"));
            let b = add(&engine, &server, "/b.bin", &root.join("out"));

            thread::sleep(SETTLE);
            assert!(server.hits.lock().unwrap().is_empty());
            assert_eq!(job_state(&engine, &a), JobState::Queued);
            assert_eq!(job_state(&engine, &b), JobState::Queued);

            switch(&sensors, battery_saver, false);
            wait_until("both downloads to start", || {
                job_state(&engine, &a).is_active() && job_state(&engine, &b).is_active()
            });
            server.release();
            wait_until("both downloads to finish", || {
                job_state(&engine, &a) == JobState::Completed
                    && job_state(&engine, &b) == JobState::Completed
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn start_now_runs_a_job_through_a_hold_and_the_hold_does_not_stop_it() {
        let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
        let server = FileServer::start(vec![
            ("/now.bin".into(), vec![1; 4096], true),
            ("/later.bin".into(), vec![2; 4096], true),
        ]);
        let sensors = FakeSensors::default();
        switch(&sensors, true, true);
        let engine = open(&root, &sensors);
        wait_for_hold(&engine, true);
        let now = add(&engine, &server, "/now.bin", &root.join("out"));
        let later = add(&engine, &server, "/later.bin", &root.join("out"));

        engine.start_now(&now).unwrap();
        wait_until("the forced download to start", || {
            job_state(&engine, &now).is_active()
        });
        thread::sleep(SETTLE);
        assert!(job_state(&engine, &now).is_active());
        assert_eq!(job_state(&engine, &later), JobState::Queued);
        assert_eq!(requests(&server, "/later.bin"), 0);

        server.release();
        wait_until("the forced download to finish", || {
            job_state(&engine, &now) == JobState::Completed
        });
        thread::sleep(SETTLE);
        assert_eq!(job_state(&engine, &later), JobState::Queued);
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_running_live_recording_keeps_running_through_a_hold_and_a_live_job_not_yet_started_waits()
    {
        for battery_saver in [false, true] {
            let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
            let server = FileServer::start(vec![
                ("/live.bin".into(), vec![6; 4096], true),
                ("/other.bin".into(), vec![7; 4096], true),
            ]);
            let sensors = FakeSensors::default();
            switch(&sensors, battery_saver, true);
            let engine = open(&root, &sensors);
            wait_for_hold(&engine, true);
            let live = add(&engine, &server, "/live.bin", &root.join("out"));
            mark_as_live_recording(&live);
            let other = add(&engine, &server, "/other.bin", &root.join("out"));

            thread::sleep(SETTLE);
            assert!(server.hits.lock().unwrap().is_empty());
            assert_eq!(job_state(&engine, &live), JobState::Queued);

            switch(&sensors, battery_saver, false);
            wait_until("both downloads to start", || {
                job_state(&engine, &live).is_active() && job_state(&engine, &other).is_active()
            });
            switch(&sensors, battery_saver, true);
            wait_until("the hold to send the other job back", || {
                job_state(&engine, &other) == JobState::Queued
            });
            let started = requests(&server, "/live.bin");
            thread::sleep(SETTLE);
            assert!(job_state(&engine, &live).is_active());
            assert_eq!(requests(&server, "/live.bin"), started);
            assert_eq!(job_state(&engine, &other), JobState::Queued);

            server.release();
            wait_until("the live recording to finish", || {
                job_state(&engine, &live) == JobState::Completed
            });
            engine.shutdown();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn the_hold_applies_at_startup_before_any_queued_job_starts() {
        let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
        let server = FileServer::start(vec![("/waiting.bin".into(), vec![3; 4096], true)]);
        let sensors = FakeSensors::default();
        switch(&sensors, false, true);
        let engine = open(&root, &sensors);
        wait_for_hold(&engine, true);
        let id = add(&engine, &server, "/waiting.bin", &root.join("out"));
        engine.shutdown();

        let restarted = FakeSensors::default();
        switch(&restarted, false, true);
        let engine = Engine::open_with(
            AppPaths::new(root.clone()).unwrap(),
            Box::new(restarted.clone()),
            POLL,
        )
        .unwrap();
        wait_for_hold(&engine, true);
        thread::sleep(SETTLE);
        assert!(server.hits.lock().unwrap().is_empty());
        assert_eq!(job_state(&engine, &id), JobState::Queued);

        switch(&restarted, false, false);
        wait_until("the queued job to start", || {
            job_state(&engine, &id).is_active()
        });
        server.release();
        wait_until("the download to finish", || {
            job_state(&engine, &id) == JobState::Completed
        });
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hold_does_not_use_up_the_retry_budget() {
        let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
        let server = FileServer::start(vec![("/held.bin".into(), vec![4; 4096], true)]);
        let sensors = FakeSensors::default();
        let engine = open(&root, &sensors);
        let mut settings = engine.snapshot().settings;
        settings.retry_limit = 0;
        engine.update_settings(settings).unwrap();
        let id = add(&engine, &server, "/held.bin", &root.join("out"));
        wait_until("the download to start", || {
            job_state(&engine, &id).is_active()
        });

        for _ in 0..2 {
            switch(&sensors, false, true);
            wait_until("the hold to send the job back to the queue", || {
                job_state(&engine, &id) == JobState::Queued
            });
            thread::sleep(SETTLE);
            let held = job(&engine, &id);
            assert_eq!((held.state, held.attempts), (JobState::Queued, 0));
            assert_eq!(held.error, None);

            switch(&sensors, false, false);
            wait_until("the job to run again", || {
                job_state(&engine, &id).is_active()
            });
        }
        server.release();
        wait_until("the download to finish", || {
            job_state(&engine, &id).is_terminal()
        });
        let finished = job(&engine, &id);
        assert_eq!(finished.state, JobState::Completed, "{:?}", finished.error);
        assert_eq!(finished.attempts, 0);
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn both_guards_persist_and_a_profile_saved_without_the_battery_saver_one_loads_with_it_off() {
        let root = std::env::temp_dir().join(format!("ssdownload-hold-{}", Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        let engine = Engine::open(paths.clone()).unwrap();
        let settings = engine.snapshot().settings;
        assert!(!settings.pause_on_metered && !settings.pause_on_battery_saver);
        engine.shutdown();

        let engine = open(&root, &FakeSensors::default());
        engine.shutdown();
        let mut store = Store::open(&paths.database).unwrap();
        let saved = store.load_settings().unwrap().unwrap();
        assert!(saved.pause_on_metered && saved.pause_on_battery_saver);

        let mut value = serde_json::to_value(saved).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("pause_on_battery_saver");
        store
            .save_settings(&serde_json::from_value(value).unwrap())
            .unwrap();
        drop(store);
        let engine = Engine::open(paths).unwrap();
        let settings = engine.snapshot().settings;
        assert!(settings.pause_on_metered && !settings.pause_on_battery_saver);
        engine.shutdown();
        fs::remove_dir_all(root).unwrap();
    }
}
