use super::*;
use std::fs;

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
        .begin_browser_transfer(&id, std::process::id(), sender_started_at)
        .unwrap();
    assert!(engine.browser_transfer_progress(&id, 1, Some(2)).is_err());
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
        .begin_browser_transfer(&id, std::process::id(), sender_started_at)
        .unwrap();
    engine
        .release_browser_transfer(&id, first_generation)
        .unwrap();
    let paused = engine.snapshot();
    let job = paused.jobs.iter().find(|job| job.id == id).unwrap();
    assert_eq!(job.state, JobState::Paused);
    assert!(job.phase.contains("bağlantısı koptu"));

    let (_, current_generation) = engine
        .begin_browser_transfer(&id, std::process::id(), sender_started_at)
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
        .begin_browser_transfer(&id, sender.id(), sender_started_at)
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
        .begin_browser_transfer(&id, std::process::id(), sender_started_at)
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
