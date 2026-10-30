//! Behaviour pins and a local benchmark that drive `download` against the configurable
//! loopback server in `crate::test_http`.

use super::*;
use crate::test_http::{
    file_sha256, pattern_sha256, Config, CutBody, Logged, RangeSupport, Server, TempDir,
};

const MIB: u64 = 1024 * 1024;

fn request_for(server: &Server) -> AddRequest {
    AddRequest {
        url: server.url("/payload.bin"),
        ..Default::default()
    }
}

fn fetch(
    request: &AddRequest,
    output: &Path,
    control: &TransferControl,
    connections: u8,
    per_host_limit: u8,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    download(
        request,
        output,
        control,
        &NetworkGovernor::new(per_host_limit),
        connections,
        None,
        progress,
    )
}

/// Half-open byte ranges of the ranged GETs, without the one-byte range-support probe.
fn ranged_gets(log: &[Logged], total: u64) -> Vec<(u64, u64)> {
    log.iter()
        .filter(|entry| entry.method == "GET")
        .filter_map(|entry| entry.range.as_deref())
        .filter(|range| *range != "bytes=0-0")
        .map(|range| {
            let (start, end) = range
                .strip_prefix("bytes=")
                .and_then(|spec| spec.split_once('-'))
                .unwrap();
            let end = if end.is_empty() {
                total
            } else {
                end.parse::<u64>().unwrap() + 1
            };
            (start.parse().unwrap(), end)
        })
        .collect()
}

/// Asserts that the ranges cover `0..total` once each, with no gap and no overlap.
fn assert_tiles(mut ranges: Vec<(u64, u64)>, total: u64) {
    ranges.sort_unstable();
    let mut next = 0;
    for (start, end) in &ranges {
        assert_eq!(*start, next, "ranges {ranges:?}");
        next = *end;
    }
    assert_eq!(next, total, "ranges {ranges:?}");
}

#[test]
fn segmented_download_over_several_connections_has_exactly_the_source_bytes() {
    let size = 24 * MIB + 12_345;
    let server = Server::start(Config {
        size,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("segmented");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.bytes, size);
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());

    let log = server.log();
    assert_tiles(ranged_gets(&log, size), size);
    let mut connections = log
        .iter()
        .filter(|entry| entry.method == "GET" && entry.range.is_some())
        .map(|entry| entry.connection)
        .collect::<Vec<_>>();
    connections.sort_unstable();
    connections.dedup();
    assert!(
        connections.len() > 1,
        "ranges used connections {connections:?}"
    );
}

#[test]
fn an_interrupted_segmented_download_resumes_by_requesting_only_the_missing_ranges_with_if_range() {
    let size = 40 * MIB + 4_097;
    let server = Server::start(Config {
        size,
        throttle: Some(32 * MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("segmented-resume");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 2, 2, &mut |_| {
        if read_state(&state_file).is_some_and(|state| !state.completed.is_empty()) {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let kept = read_state(&state_file).unwrap();
    let done = kept
        .completed
        .iter()
        .map(|range| (range.start, range.end))
        .collect::<Vec<_>>();
    assert!(
        !done.is_empty() && done.iter().map(|(start, end)| end - start).sum::<u64>() < size,
        "the interruption left {done:?} of {size} bytes"
    );
    assert!(part_path(&output).exists() && !output.exists());
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));

    let log = server.log();
    let resumed = &log[first_attempt..];
    let mut requested = ranged_gets(resumed, size);
    assert!(!requested.is_empty());
    for entry in resumed.iter().filter(|entry| entry.method == "GET") {
        assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
    }
    requested.extend(done);
    assert_tiles(requested, size);
}

#[test]
fn a_source_that_changes_between_attempts_restarts_and_publishes_the_new_version() {
    let size = MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(MIB),
        switch_version_after: Some(2),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("changed-source");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    assert!(fs::metadata(part_path(&output)).unwrap().len() > 0);
    let first_attempt = server.log();
    assert_eq!(
        first_attempt
            .iter()
            .map(|entry| entry.method.as_str())
            .collect::<Vec<_>>(),
        ["HEAD", "GET"]
    );

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(result.bytes, size);
    assert_eq!(file_sha256(&result.path), pattern_sha256(2, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    let log = server.log();
    let second_attempt = &log[first_attempt.len()..];
    assert_eq!(second_attempt[0].method, "HEAD");
    assert!(
        second_attempt
            .iter()
            .all(|entry| entry.range.is_none() && entry.if_range.is_none()),
        "the restart asked for old bytes: {second_attempt:?}"
    );
    assert_eq!(
        second_attempt.iter().filter(|e| e.method == "GET").count(),
        1
    );
}

#[test]
fn a_server_that_ignores_range_gets_one_plain_download() {
    let size = 6 * MIB + 321;
    let server = Server::start(Config {
        size,
        ranges: RangeSupport::Ignore,
        ..Config::default()
    });
    let directory = TempDir::new("ignored-range");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());

    let log = server.log();
    assert_eq!(ranged_gets(&log, size), []);
    let bodies = log
        .iter()
        .filter(|entry| entry.method == "GET" && entry.range.is_none())
        .collect::<Vec<_>>();
    assert_eq!(bodies.len(), 1, "{log:?}");
}

#[test]
fn an_existing_file_at_the_target_name_is_kept_and_the_download_gets_another_name() {
    let size = 3_000;
    let server = Server::start(Config {
        size,
        ..Config::default()
    });
    let directory = TempDir::new("existing-target");
    let output = directory.0.join("payload.bin");
    fs::write(&output, b"user data").unwrap();

    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(fs::read(&output).unwrap(), b"user data");
    assert_ne!(result.path, output);
    assert_eq!(result.path.parent(), output.parent());
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
}

const BENCH_SIZE: u64 = 128 * MIB;
const BENCH_RATE: u64 = 4 * MIB;
/// The first range worker's socket: connection 0 answers the HEAD probe and 1 the range probe.
const SLOW_CONNECTION: usize = 2;

/// Downloads the benchmark file with the connection values the engine would pass to
/// `download` and prints seconds and MiB/s. A failed attempt starts the next one at once,
/// like the engine's job retry without its delay.
fn bench(label: &str, server_config: Config, connections: u8, per_host_limit: u8) {
    let server = Server::start(Config {
        size: BENCH_SIZE,
        ..server_config
    });
    let request = request_for(&server);
    let directory = TempDir::new("bench");
    let output = directory.0.join("payload.bin");
    let control = TransferControl::default();
    let network = NetworkGovernor::new(per_host_limit);
    let started = Instant::now();
    let mut attempts = 0;
    let result = loop {
        attempts += 1;
        match download(
            &request,
            &output,
            &control,
            &network,
            connections,
            None,
            &mut |_| {},
        ) {
            Ok(result) => break result,
            Err(error) => assert!(attempts < 50, "{label}: {error:#}"),
        }
    };
    let seconds = started.elapsed().as_secs_f64();
    assert_eq!(
        file_sha256(&result.path),
        pattern_sha256(1, BENCH_SIZE),
        "{label}"
    );
    let log = server.log();
    let sockets = log
        .iter()
        .filter(|entry| entry.method == "GET")
        .map(|entry| entry.connection)
        .collect::<std::collections::BTreeSet<_>>();
    println!(
        "bench {label}: {seconds:.1} s, {:.2} MiB/s, {attempts} attempt(s), {} requests, {} data connection(s), {} cut",
        BENCH_SIZE as f64 / MIB as f64 / seconds,
        log.len(),
        sockets.len(),
        log.iter().filter(|entry| entry.body_cut).count(),
    );
}

fn throttled() -> Config {
    Config {
        throttle: Some(BENCH_RATE),
        ..Config::default()
    }
}

fn default_settings() -> (u8, u8) {
    let settings = crate::model::Settings::default();
    (settings.connections, settings.per_host_limit)
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_default_connections() {
    let (connections, per_host_limit) = default_settings();
    bench(
        "default connections, 4 MiB/s each",
        throttled(),
        connections,
        per_host_limit,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_eight_connections() {
    let (_, per_host_limit) = default_settings();
    bench(
        "8 connections requested, 4 MiB/s each",
        throttled(),
        8,
        per_host_limit,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_one_slow_connection() {
    let (_, per_host_limit) = default_settings();
    bench(
        "8 connections requested, one at 0.5 MiB/s",
        Config {
            slow_connection: Some((SLOW_CONNECTION, MIB / 2)),
            ..throttled()
        },
        8,
        per_host_limit,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_dropped_connections() {
    let (_, per_host_limit) = default_settings();
    bench(
        "8 connections requested, first 6 responses cut",
        Config {
            cut_body: Some(CutBody {
                after_bytes: 4 * MIB,
                responses: 6,
            }),
            ..throttled()
        },
        8,
        per_host_limit,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_ranges_without_accept_ranges_header() {
    let (_, per_host_limit) = default_settings();
    bench(
        "8 connections requested, no Accept-Ranges header",
        Config {
            accept_ranges_header: false,
            ..throttled()
        },
        8,
        per_host_limit,
    );
}
