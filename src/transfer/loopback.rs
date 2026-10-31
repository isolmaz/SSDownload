//! Behaviour pins and a local benchmark that drive `download` against the configurable
//! loopback server in `crate::test_http`.

use super::*;
use crate::test_http::{
    file_sha256, pattern, pattern_sha256, Config, CutBody, EtagKind, Logged, RangeSupport,
    RetryAfter, Scripted, Sequence, Server, TempDir,
};

const MIB: u64 = 1024 * 1024;
const NEVER: Duration = Duration::from_secs(3600);

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

/// The GETs that carry file data: all but the one-byte range-support probe.
fn data_gets(log: &[Logged]) -> impl Iterator<Item = &Logged> {
    log.iter()
        .filter(|entry| entry.method == "GET" && entry.range.as_deref() != Some("bytes=0-0"))
}

/// Half-open byte ranges of the ranged data GETs.
fn ranged_gets(log: &[Logged], total: u64) -> Vec<(u64, u64)> {
    data_gets(log)
        .filter_map(|entry| entry.range.as_deref())
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

/// The half-open ranges that the ranged data GETs delivered. A request delivers only up to the
/// start of another one that begins inside it, whichever was sent first: a lease that was cut
/// for an idle worker, or one that dropped and was asked for again from where it stopped. The
/// request log alone cannot show a byte delivered twice; `assert_written_once` does.
fn delivered(log: &[Logged], total: u64) -> Vec<(u64, u64)> {
    let requested = ranged_gets(log, total);
    requested
        .iter()
        .map(|&(start, end)| {
            let cut = requested
                .iter()
                .map(|&(other, _)| other)
                .filter(|&other| other > start && other < end)
                .min();
            (start, cut.unwrap_or(end))
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

/// The offset and length of every write that the segmented workers of the downloads that the
/// current test thread ran made.
fn writes() -> Vec<(u64, u64)> {
    TEST_WRITES.with(|writes| writes.lock().unwrap().clone())
}

fn forget_writes() {
    TEST_WRITES.with(|writes| writes.lock().unwrap().clear());
}

/// Asserts that no byte was written twice. The bytes of a file cannot show it: every write of a
/// byte carries the same value.
fn assert_no_byte_written_twice(writes: &[(u64, u64)]) {
    let mut writes = writes.to_vec();
    writes.sort_unstable();
    let mut next = 0;
    for (offset, length) in writes {
        assert!(offset >= next, "bytes {offset}..{next} were written twice");
        next = offset + length;
    }
}

/// Asserts that `writes` and the `recorded` ranges that an earlier attempt kept cover `0..total`
/// once each: every byte that no record held was written, and none twice.
fn assert_written_once(writes: &[(u64, u64)], recorded: &[(u64, u64)], total: u64) {
    let mut tiles = writes
        .iter()
        .map(|&(offset, length)| (offset, offset + length))
        .chain(recorded.iter().copied())
        .collect::<Vec<_>>();
    tiles.sort_unstable();
    let mut next = 0;
    for (start, end) in tiles {
        assert!(
            start >= next,
            "bytes {start}..{next} were written or recorded twice"
        );
        assert!(start <= next, "bytes {next}..{start} were never written");
        next = end;
    }
    assert_eq!(next, total, "bytes {next}..{total} were never written");
}

/// A range that a worker took from the back of another range.
#[derive(Debug)]
struct Steal {
    /// The range that was requested first and still has the same end.
    victim: (u64, u64),
    thief: (u64, u64),
    /// Whether the request for `victim` is logged before the one for `thief`.
    victim_first: bool,
}

/// Every pair of ranged data GETs of which one asks for a part that ends where the other ends
/// and starts strictly inside it.
fn steals(log: &[Logged], total: u64) -> Vec<Steal> {
    let ranges = ranged_gets(log, total);
    let mut steals = Vec::new();
    for (thief_index, &thief) in ranges.iter().enumerate() {
        for (victim_index, &victim) in ranges.iter().enumerate() {
            if victim.0 < thief.0 && thief.0 < victim.1 && thief.1 == victim.1 {
                steals.push(Steal {
                    victim,
                    thief,
                    victim_first: victim_index < thief_index,
                });
            }
        }
    }
    steals
}

/// Whether a progress phase says that a range is tried again.
fn is_retry_notice(phase: &str) -> bool {
    phase.contains("retrying") || phase.contains("yeniden")
}

/// Asserts that the file at `path` holds exactly `expected`.
fn assert_file_is(path: &Path, expected: &[u8], label: &str) {
    assert!(
        fs::read(path).unwrap() == expected,
        "{label}: the file is not the source"
    );
}

/// What a download left behind on a server whose first connection that starts above byte 0
/// stops after some bytes until another request takes part of its range.
struct Stalled {
    /// The file the download produced.
    file: Vec<u8>,
    log: Vec<Logged>,
    writes: Vec<(u64, u64)>,
    phases: Vec<String>,
}

/// Downloads `size` bytes over `connections` connections.
fn download_with_a_stall(size: u64, connections: u8, after: u64, throttle: Option<u64>) -> Stalled {
    let server = Server::start(Config {
        size,
        throttle,
        stall: Some((0, after)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("stall");
    let output = directory.0.join("payload.bin");
    let mut phases = Vec::new();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |progress| phases.push(progress.phase),
    )
    .unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(result.bytes, size);
    let file = fs::read(&result.path).unwrap();
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    server.wait_idle();
    Stalled {
        file,
        log: server.log(),
        writes: writes(),
        phases,
    }
}

impl Stalled {
    /// The file is the source, every byte was written once, the requests tile the file, each of
    /// them carried the validator of the record and was answered with a partial body, and no
    /// range was tried again.
    fn assert_exact(&self, expected: &[u8]) {
        let size = expected.len() as u64;
        assert!(self.file == expected, "the file is not the source");
        assert_written_once(&self.writes, &[], size);
        assert_tiles(delivered(&self.log, size), size);
        for entry in data_gets(&self.log) {
            assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
            assert_eq!(entry.status, 206, "{entry:?}");
        }
        assert!(
            self.phases.iter().all(|phase| !is_retry_notice(phase)),
            "{:?}",
            self.phases
        );
    }
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
    forget_writes();
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
    assert_written_once(&writes(), &[], size);

    let log = server.log();
    assert_tiles(delivered(&log, size), size);
    let mut connections = data_gets(&log)
        .filter(|entry| entry.range.is_some())
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
    let interrupted = fetch(&request, &output, &control, 2, 2, &mut |progress| {
        if progress.downloaded > 0 {
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
    // Only the recorded ranges are retained, not the preallocated part file; a stop records
    // what the running leases had written.
    let recorded = done.iter().map(|(start, end)| end - start).sum::<u64>();
    assert_eq!(retained_bytes(&output), Some(recorded));
    assert!(recorded < fs::metadata(part_path(&output)).unwrap().len());
    assert!(!control.restarted());
    server.wait_idle();
    let first_attempt = server.log().len();

    forget_writes();
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
    assert_written_once(&writes(), &done, size);

    let log = server.log();
    let resumed = &log[first_attempt..];
    let mut requested = delivered(resumed, size);
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
    assert_eq!(
        retained_bytes(&output),
        Some(fs::metadata(part_path(&output)).unwrap().len())
    );
    assert!(!control.restarted());
    server.wait_idle();
    let first_attempt = server.log();
    assert_eq!(
        first_attempt
            .iter()
            .map(|entry| entry.method.as_str())
            .collect::<Vec<_>>(),
        ["HEAD", "GET"]
    );

    let restarting = TransferControl::default();
    let result = fetch(&request, &output, &restarting, 1, 1, &mut |_| {}).unwrap();
    assert!(restarting.restarted());
    assert_eq!(result.bytes, size);
    assert_eq!(file_sha256(&result.path), pattern_sha256(2, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    let log = server.log();
    let second_attempt = &log[first_attempt.len()..];
    assert_eq!(second_attempt[0].method, "HEAD");
    // The stored validator is offered once with the resume request; the new version answers it
    // with a full body, so no old byte is accepted and the restart is a single plain GET.
    let ranged = second_attempt
        .iter()
        .filter(|entry| entry.range.is_some())
        .collect::<Vec<_>>();
    assert!(
        ranged.len() <= 1
            && ranged
                .iter()
                .all(|entry| entry.if_range.as_deref() == Some("\"v1\"") && entry.status == 200),
        "{second_attempt:?}"
    );
    assert_eq!(
        second_attempt
            .iter()
            .filter(|e| e.method == "GET" && e.range.is_none())
            .count(),
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
fn a_server_that_serves_ranges_without_advertising_them_is_downloaded_over_several_connections() {
    let size = 24 * MIB + 12_345;
    let server = Server::start(Config {
        size,
        accept_ranges_header: false,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("unadvertised-ranges");
    let output = directory.0.join("payload.bin");
    forget_writes();
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
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    let ranged = ranged_gets(&log, size);
    assert_tiles(delivered(&log, size), size);
    let connections = data_gets(&log)
        .map(|entry| entry.connection)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        connections.len() > 1,
        "ranges used connections {connections:?}"
    );
    // One HEAD and one one-byte probe, then only the ranges themselves.
    assert_eq!(log.iter().filter(|e| e.method == "HEAD").count(), 1);
    let probes = log
        .iter()
        .filter(|e| e.range.as_deref() == Some("bytes=0-0"))
        .collect::<Vec<_>>();
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].status, 206);
    assert_eq!(log.len(), 2 + ranged.len(), "{log:?}");
}

#[test]
fn a_range_probe_answered_with_200_is_abandoned_so_the_body_is_sent_once() {
    let size = 16 * MIB;
    for advertised in [true, false] {
        let server = Server::start(Config {
            size,
            ranges: RangeSupport::Ignore,
            accept_ranges_header: advertised,
            throttle: Some(32 * MIB),
            ..Config::default()
        });
        let directory = TempDir::new("range-probe-200");
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

        server.wait_idle();
        let log = server.log();
        assert_eq!(ranged_gets(&log, size), [], "{log:?}");
        assert_eq!(data_gets(&log).count(), 1, "{log:?}");
        assert!(log
            .iter()
            .filter(|e| e.method == "GET")
            .all(|e| e.status == 200));
        // The abandoned probe may have left a few chunks in flight, never the whole body.
        let sent = server.body_bytes_sent();
        assert!(
            (size..size + 4 * MIB).contains(&sent),
            "the server wrote {sent} body bytes for a {size} byte file (header advertised: {advertised})"
        );
    }
}

#[test]
fn a_head_with_the_wrong_length_does_not_decide_the_size() {
    let size = 6 * MIB + 321;
    for (head_length, segmented) in [(7, false), (100 * MIB, true)] {
        let server = Server::start(Config {
            size,
            head_content_length: Some(head_length),
            throttle: Some(64 * MIB),
            ..Config::default()
        });
        let directory = TempDir::new("head-length");
        let output = directory.0.join("payload.bin");
        forget_writes();
        let result = fetch(
            &request_for(&server),
            &output,
            &TransferControl::default(),
            4,
            4,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("HEAD length {head_length}: {error:#}"));
        assert_eq!(result.bytes, size);
        assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        if segmented {
            assert_written_once(&writes(), &[], size);
            server.wait_idle();
            assert_tiles(delivered(&server.log(), size), size);
        }
    }
}

#[test]
fn a_server_that_refuses_head_is_downloaded_over_ranges() {
    let size = 12 * MIB + 99;
    let server = Server::start(Config {
        size,
        head_not_allowed: true,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("head-refused");
    let output = directory.0.join("payload.bin");
    forget_writes();
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
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    assert_eq!(log[0].method, "HEAD");
    assert_eq!(log[0].status, 405);
    assert_tiles(delivered(&log, size), size);
}

#[test]
fn a_fresh_download_adopts_the_validators_of_the_response_that_delivers_the_bytes() {
    let size = 12 * MIB + 7;
    for connections in [1, 4] {
        let server = Server::start(Config {
            size,
            vary_etag: true,
            throttle: Some(64 * MIB),
            ..Config::default()
        });
        let directory = TempDir::new("varying-etag");
        let output = directory.0.join("payload.bin");
        let result = fetch(
            &request_for(&server),
            &output,
            &TransferControl::default(),
            connections,
            connections,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{connections} connection(s): {error:#}"));
        assert_eq!(
            file_sha256(&result.path),
            pattern_sha256(1, size),
            "{connections} connection(s)"
        );
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
    }
}

#[test]
fn the_state_of_an_interrupted_fresh_download_names_the_response_that_wrote_the_bytes() {
    let size = MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(MIB),
        vary_etag: true,
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("varying-etag-resume");
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
    server.wait_idle();
    let first_attempt = server.log();
    assert_eq!(
        first_attempt
            .iter()
            .map(|entry| entry.method.as_str())
            .collect::<Vec<_>>(),
        ["HEAD", "GET"]
    );
    // The server numbers its ETags by request: the HEAD was request 0, the GET request 1.
    assert_eq!(
        read_state(&state_path(&output)).unwrap().etag.as_deref(),
        Some("\"v1-1\"")
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
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    // Every response carries a new ETag, so the stored one is refused (200) and no old byte is
    // accepted: the restart is one plain GET.
    let second = &log[first_attempt.len()..];
    assert!(
        second
            .iter()
            .filter(|entry| entry.range.is_some())
            .all(|entry| entry.if_range.as_deref() == Some("\"v1-1\"") && entry.status == 200),
        "{second:?}"
    );
    assert_eq!(
        second
            .iter()
            .filter(|entry| entry.method == "GET" && entry.range.is_none())
            .count(),
        1
    );
}

#[test]
fn a_fresh_probe_carries_no_if_range_so_a_head_with_other_validators_still_segments() {
    let size = 24 * MIB + 5;
    let server = Server::start(Config {
        size,
        head_etag: Some("\"h\"".into()),
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("head-validators");
    let output = directory.0.join("payload.bin");
    forget_writes();
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
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    let probe = log
        .iter()
        .find(|entry| entry.range.as_deref() == Some("bytes=0-0"))
        .unwrap();
    assert_eq!((probe.if_range.as_deref(), probe.status), (None, 206));
    assert_tiles(delivered(&log, size), size);
    // The ranges are bound to the validator of the GET that answered the probe, not HEAD's.
    for entry in data_gets(&log) {
        assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
        assert_eq!(entry.status, 206, "{entry:?}");
    }
    let connections = data_gets(&log)
        .map(|entry| entry.connection)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(connections.len() > 1, "{connections:?}");
}

#[test]
fn a_head_without_length_or_validators_does_not_prevent_the_range_probe() {
    let size = 24 * MIB + 5;
    let server = Server::start(Config {
        size,
        head_bare: true,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("bare-head");
    let output = directory.0.join("payload.bin");
    forget_writes();
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
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    assert_tiles(delivered(&log, size), size);
    let connections = data_gets(&log)
        .map(|entry| entry.connection)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(connections.len() > 1, "{connections:?}");
}

#[test]
fn a_full_response_without_a_length_is_not_measured_against_the_head_length() {
    let size = 3 * MIB + 11;
    let server = Server::start(Config {
        size,
        head_content_length: Some(7),
        get_length_header: false,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("unframed-get");
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
}

#[test]
fn progress_reports_the_size_that_the_delivering_response_states() {
    let size = 2 * MIB;
    let server = Server::start(Config {
        size,
        head_content_length: Some(7),
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("progress-total");
    let output = directory.0.join("payload.bin");
    let mut totals = Vec::new();
    fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |progress| totals.push(progress.total),
    )
    .unwrap();
    assert!(!totals.is_empty());
    assert!(
        totals.iter().all(|total| *total == Some(size)),
        "{totals:?}"
    );
}

#[test]
fn a_retry_notice_does_not_report_the_head_length_as_the_size() {
    let size = MIB;
    // The empty 200 is the answer to the HEAD and says the file is empty; the first GET is
    // then refused and retried.
    let server = Server::start(Config {
        size,
        scripted: vec![
            Scripted {
                status: 200,
                retry_after: None,
            },
            Scripted {
                status: 429,
                retry_after: Some(RetryAfter::Seconds(0)),
            },
        ],
        ..Config::default()
    });
    let directory = TempDir::new("retry-total");
    let output = directory.0.join("payload.bin");
    let mut notices = Vec::new();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |progress| notices.push((progress.total, progress.phase.to_string())),
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    let retry = notices
        .iter()
        .find(|(_, phase)| phase.contains("429"))
        .unwrap_or_else(|| panic!("no retry notice in {notices:?}"));
    assert_eq!(retry.0, None);
}

#[test]
fn a_single_connection_download_resumes_when_the_head_disagrees_with_the_get() {
    let size = MIB;
    let server = Server::start(Config {
        size,
        head_etag: Some("\"h\"".into()),
        throttle: Some(MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("resume-head-differs");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let kept = fs::metadata(part_path(&output)).unwrap().len();
    assert!(kept > 0 && kept < size);
    server.wait_idle();
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    let resumed = log[first_attempt..]
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(resumed.len(), 1, "{log:?}");
    assert_eq!(resumed[0].range, Some(format!("bytes={kept}-")));
    assert_eq!(resumed[0].if_range.as_deref(), Some("\"v1\""));
    assert_eq!(resumed[0].status, 206);
}

#[test]
fn an_interrupted_segmented_download_resumes_when_the_head_disagrees_with_the_get() {
    let size = 40 * MIB + 4_097;
    let server = Server::start(Config {
        size,
        head_etag: Some("\"h\"".into()),
        throttle: Some(32 * MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("segmented-resume-head-differs");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 2, 2, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let done = read_state(&state_file)
        .unwrap()
        .completed
        .iter()
        .map(|range| (range.start, range.end))
        .collect::<Vec<_>>();
    assert!(!done.is_empty() && done.iter().map(|(start, end)| end - start).sum::<u64>() < size);
    server.wait_idle();
    let first_attempt = server.log().len();

    forget_writes();
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
    assert_written_once(&writes(), &done, size);
    server.wait_idle();
    let log = server.log();
    let resumed = &log[first_attempt..];
    let mut requested = delivered(resumed, size);
    assert!(!requested.is_empty());
    for entry in resumed.iter().filter(|entry| entry.method == "GET") {
        assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
        assert_eq!(entry.status, 206, "{entry:?}");
    }
    requested.extend(done);
    assert_tiles(requested, size);
}

#[test]
fn resuming_against_a_server_that_ignores_ranges_asks_for_a_range_once() {
    let size = 8 * MIB;
    let server = Server::start(Config {
        size,
        ranges: RangeSupport::Ignore,
        accept_ranges_header: false,
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("resume-ignored-ranges");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 4, 4, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let kept = fs::metadata(part_path(&output)).unwrap().len();
    assert!(kept > 0 && kept < size);
    server.wait_idle();
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    let second = &log[first_attempt..];
    // HEAD, the resume request (answered with the full body and abandoned), then one plain GET:
    // the 200 already showed that ranges are ignored, so no one-byte probe follows.
    assert_eq!(
        second
            .iter()
            .map(|entry| (
                entry.method.as_str(),
                entry.range.as_deref().map(str::to_owned),
                entry.status
            ))
            .collect::<Vec<_>>(),
        [
            ("HEAD", None, 200),
            ("GET", Some(format!("bytes={kept}-")), 200),
            ("GET", None, 200)
        ]
    );
    assert_eq!(second[1].if_range.as_deref(), Some("\"v1\""));
}

/// Version 2 replaces the file between two attempts with the same size, and the server
/// answers the resume without any proof of identity that the changed file could not give too.
/// Whatever the weakness is (`tweak`), the second attempt has to restart and publish version 2.
fn swapped_file_restarts(tweak: impl Fn(&mut Config), digest_of_version_2: bool) {
    for (connections, size, rate) in [(1u8, MIB, MIB), (2, 40 * MIB + 4_097, 32 * MIB)] {
        swapped_file_restarts_over(connections, size, rate, &tweak, digest_of_version_2);
    }
}

#[test]
fn a_same_size_changed_file_is_not_resumed_into_the_old_partial_over_one_connection() {
    swapped_file_restarts_over(
        1,
        MIB,
        MIB,
        &|config| {
            config.ignore_if_range = true;
            config.partial_without_validators = true;
        },
        false,
    );
}

#[test]
fn a_same_size_changed_file_is_not_resumed_into_the_old_partial_over_several_connections() {
    swapped_file_restarts_over(
        2,
        40 * MIB + 4_097,
        32 * MIB,
        &|config| {
            config.ignore_if_range = true;
            config.partial_without_validators = true;
        },
        false,
    );
}

#[test]
fn a_matching_last_modified_does_not_prove_a_file_whose_etag_changed() {
    // Strong ETag, `If-Range` ignored, no validator on the resume answer, the same
    // `Last-Modified` on both versions: only the ETag tells them apart.
    swapped_file_restarts(
        |config| {
            config.ignore_if_range = true;
            config.partial_without_validators = true;
            config.last_modified_fixed = true;
        },
        false,
    );
}

#[test]
fn a_weak_etag_that_changed_is_not_hidden_by_a_matching_last_modified() {
    // `If-Range` carries the date, which still matches, so the server honours it; the answer
    // has no validator, and HEAD's changed weak ETag is the only sign of the new file.
    swapped_file_restarts(
        |config| {
            config.etag = EtagKind::Weak;
            config.last_modified_fixed = true;
            config.partial_without_validators = true;
        },
        false,
    );
}

#[test]
fn a_strong_etag_dropped_from_an_answer_that_keeps_last_modified_does_not_prove_the_file() {
    // A server that honoured `If-Range` would answer with the whole new file.
    swapped_file_restarts(
        |config| {
            config.ignore_if_range = true;
            config.partial_without_etag = true;
            config.last_modified_fixed = true;
        },
        false,
    );
}

#[test]
fn a_weak_etag_dropped_from_an_answer_that_keeps_last_modified_does_not_prove_the_file() {
    swapped_file_restarts(
        |config| {
            config.etag = EtagKind::Weak;
            config.partial_without_etag = true;
            config.last_modified_fixed = true;
        },
        false,
    );
}

#[test]
fn a_supplied_digest_does_not_vouch_against_a_conflicting_strong_etag() {
    // The digest is that of version 2. Vouching for the version-1 partial would mix the two
    // and fail the final check; the conflicting HEAD ETag restarts the download instead.
    swapped_file_restarts(
        |config| {
            config.ignore_if_range = true;
            config.partial_without_validators = true;
        },
        true,
    );
}

/// Interrupts a download of version 1, swaps the file for version 2 of the same size, and
/// resumes with the server configured by `tweak` (and, with `digest_of_version_2`, a SHA-256
/// of version 2 supplied): the result has to be version 2 only, without a digest failure.
fn swapped_file_restarts_over(
    connections: u8,
    size: u64,
    rate: u64,
    tweak: &dyn Fn(&mut Config),
    digest_of_version_2: bool,
) {
    let mut config = Config {
        size,
        throttle: Some(rate),
        ..Config::default()
    };
    tweak(&mut config);
    let server = Server::start(config);
    let request = AddRequest {
        checksum: digest_of_version_2.then(|| pattern_sha256(2, size)),
        ..request_for(&server)
    };
    let directory = TempDir::new("changed-same-size");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(
        &request,
        &output,
        &control,
        connections,
        connections,
        &mut |progress| {
            if progress.downloaded > 0 {
                cancel.cancel();
            }
        },
    );
    assert!(interrupted.is_err());
    assert!(part_path(&output).exists());
    server.wait_idle();
    server.switch_version_now();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |_| {},
    )
    .unwrap_or_else(|error| panic!("{connections} connection(s): {error:#}"));
    assert_eq!(
        file_sha256(&result.path),
        pattern_sha256(2, size),
        "{connections} connection(s)"
    );
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
}

#[test]
fn a_probe_that_matches_the_record_vouches_for_a_partial_answer_without_validators() {
    let size = MIB;
    let server = Server::start(Config {
        size,
        partial_without_validators: true,
        throttle: Some(MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("head-vouches");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let kept = fs::metadata(part_path(&output)).unwrap().len();
    assert!(kept > 0 && kept < size);
    server.wait_idle();
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    let resumed = log[first_attempt..]
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(resumed.len(), 1, "{log:?}");
    assert_eq!(resumed[0].range, Some(format!("bytes={kept}-")));
    assert_eq!(resumed[0].status, 206);
}

#[test]
fn a_close_delimited_body_that_ends_before_the_head_length_is_not_published() {
    let size = 3 * MIB + 11;
    let server = Server::start(Config {
        size,
        get_length_header: false,
        cut_body: Some(CutBody {
            after_bytes: MIB,
            responses: 10,
        }),
        ..Config::default()
    });
    let directory = TempDir::new("truncated-close-delimited");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    );
    assert!(result.is_err());
    assert!(!output.exists());
    assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), MIB);
}

#[test]
fn a_single_connection_download_resumes_without_an_accept_ranges_header() {
    let size = MIB;
    let server = Server::start(Config {
        size,
        accept_ranges_header: false,
        throttle: Some(MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("resume-unadvertised");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let kept = fs::metadata(part_path(&output)).unwrap().len();
    assert!(kept > 0 && kept < size);
    server.wait_idle();
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    let resumed = log[first_attempt..]
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(resumed.len(), 1, "{log:?}");
    assert_eq!(resumed[0].range, Some(format!("bytes={kept}-")));
    assert_eq!(resumed[0].if_range.as_deref(), Some("\"v1\""));
    assert_eq!(resumed[0].status, 206);
}

#[test]
fn a_source_that_changes_after_the_probe_never_contributes_bytes_of_two_versions() {
    let size = 12 * MIB;
    // The HEAD and the one-byte probe (requests 0 and 1) see version 1; every later request,
    // all of them ranges, sees version 2.
    let server = Server::start(Config {
        size,
        switch_version_after: Some(2),
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("changed-after-probe");
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
    assert_eq!(file_sha256(&result.path), pattern_sha256(2, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
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

/// Checkpoints the downloads that the current test thread starts after `interval` or once
/// `bytes` are waiting. The cadence and the steps belong to the test thread, so tests do not
/// affect each other.
fn checkpoint_every(interval: Duration, bytes: u64) {
    TEST_CADENCE.with(|cadence| cadence.set(Some(Cadence { interval, bytes })));
}

fn steps() -> Vec<&'static str> {
    TEST_STEPS.with(|steps| steps.lock().unwrap().clone())
}

/// Makes the cycles of the downloads that the current test thread starts wait between their
/// flush and their record.
fn pause_cycles_for(pause: Duration) {
    TEST_PAUSE.with(|current| current.set(pause));
}

/// The steps of every cycle, split at the step that begins a cycle.
fn cycles() -> Vec<Vec<&'static str>> {
    let mut cycles: Vec<Vec<&'static str>> = Vec::new();
    for step in steps() {
        if step == "counted" {
            cycles.push(Vec::new());
        }
        cycles
            .last_mut()
            .expect("a cycle begins by counting")
            .push(step);
    }
    cycles
}

/// The steps of a cycle that works on `writers` handles: it flushes through the handle of its
/// own and through every writer, and only then records.
fn cycle_of(writers: u8, recorded: bool) -> Vec<&'static str> {
    let mut cycle = vec!["counted", "flushed"];
    cycle.extend(std::iter::repeat_n("writer flushed", usize::from(writers)));
    if recorded {
        cycle.push("recorded");
    }
    cycle
}

/// Cycles that flushed the part file and wrote a record.
fn checkpoint_cycles() -> usize {
    steps().iter().filter(|step| **step == "recorded").count()
}

/// Cycles that failed to flush or to write the record.
fn failed_cycles() -> usize {
    steps().iter().filter(|step| **step == "failed").count()
}

fn stop_after(control: &TransferControl, delay: Duration) -> thread::JoinHandle<()> {
    let control = control.clone();
    thread::spawn(move || {
        thread::sleep(delay);
        control.cancel();
    })
}

/// Checks every byte that `state` claims against the part file, and returns how many it claims.
fn assert_claims_are_exact(state: &ResumeState, part: &[u8], expected: &[u8]) -> u64 {
    let claimed = if state.segmented {
        state
            .completed
            .iter()
            .map(|range| (range.start as usize, range.end as usize))
            .collect::<Vec<_>>()
    } else {
        let synced = state
            .synced
            .expect("a record written by this release states the synced length");
        vec![(0, synced as usize)]
    };
    for &(start, end) in &claimed {
        assert!(
            end <= part.len() && part[start..end] == expected[start..end],
            "the record claims {start}..{end}, which the part file does not hold"
        );
    }
    claimed
        .iter()
        .map(|(start, end)| (end - start) as u64)
        .sum()
}

/// What the record of `output` claims, checked against the part file; 0 without a record.
fn claimed_bytes(output: &Path, expected: &[u8]) -> u64 {
    match read_state(&state_path(output)) {
        Some(state) => {
            let part = fs::read(part_path(output)).unwrap_or_default();
            assert_claims_are_exact(&state, &part, expected)
        }
        None => 0,
    }
}

#[test]
fn a_lease_that_drops_resumes_from_where_it_stopped() {
    let size = 24 * MIB + 12_345;
    let cut = 5 * MIB;
    let server = Server::start(Config {
        size,
        cut_body: Some(CutBody {
            after_bytes: cut,
            responses: 1,
        }),
        ..Config::default()
    });
    // No cadence checkpoint can run: the written part of the dropped lease waits for the
    // cycle at the end, and the drop itself does not flush.
    checkpoint_every(NEVER, u64::MAX);
    let directory = TempDir::new("lease-drop");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert_eq!(checkpoint_cycles(), 1, "{:?}", steps());

    server.wait_idle();
    let log = server.log();
    let ranges = ranged_gets(&log, size);
    let (start, end) = data_gets(&log)
        .filter(|entry| entry.range.is_some())
        .zip(ranges.iter())
        .find(|(entry, _)| entry.body_cut)
        .map(|(_, range)| *range)
        .expect("a lease was cut");
    let again = ranges
        .iter()
        .filter(|&&(from, to)| to == end && from > start)
        .collect::<Vec<_>>();
    assert_eq!(
        again,
        [&(start + cut, end)],
        "the cut lease {start}..{end} was requested again"
    );
    let mut covered = ranges.clone();
    let index = covered
        .iter()
        .position(|range| *range == (start, end))
        .unwrap();
    covered[index].1 = start + cut;
    assert_tiles(covered, size);
}

#[test]
fn the_number_of_checkpoints_follows_the_cadence_not_the_number_of_leases() {
    let size = 64 * MIB;
    let server = Server::start(Config {
        size,
        ..Config::default()
    });
    let bytes = 24 * MIB;
    checkpoint_every(NEVER, bytes);
    let directory = TempDir::new("checkpoint-count");
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

    server.wait_idle();
    let leases = ranged_gets(&server.log(), size).len();
    assert!(leases >= 8, "{leases} leases");
    // Every cycle but the one at the end records at least `bytes`.
    let cycles = checkpoint_cycles();
    assert!(
        cycles as u64 <= size / bytes + 1,
        "{cycles} flush and state-write cycles for {leases} leases"
    );
}

#[test]
fn the_cadence_alone_triggers_a_checkpoint_that_records_bytes_the_part_file_holds() {
    let size = 20 * MIB;
    for (connections, interval, bytes) in [
        (2u8, NEVER, MIB),
        (2, Duration::from_millis(50), u64::MAX),
        (1, NEVER, MIB),
        (1, Duration::from_millis(50), u64::MAX),
    ] {
        let label = format!("{connections} connection(s), {interval:?} or {bytes} bytes");
        let server = Server::start(Config {
            size,
            throttle: Some(4 * MIB),
            ..Config::default()
        });
        checkpoint_every(interval, bytes);
        let directory = TempDir::new("cadence");
        let output = directory.0.join("payload.bin");
        let control = TransferControl::default();
        let cancel = control.clone();
        let state_file = state_path(&output);
        let mut observed = None;
        let cycles_before = checkpoint_cycles();
        // The download takes seconds at this rate, so a checkpoint that the cadence alone
        // triggers has to come before it ends.
        let result = fetch(
            &request_for(&server),
            &output,
            &control,
            connections,
            connections,
            &mut |progress| {
                if observed.is_none()
                    && checkpoint_cycles() > cycles_before
                    && progress.downloaded < size
                {
                    // Read at once: the transfer goes on writing until it notices the stop.
                    observed = read_state(&state_file).zip(fs::read(part_path(&output)).ok());
                    cancel.cancel();
                }
            },
        );
        assert!(result.is_err(), "{label}");
        let (observed, part) =
            observed.unwrap_or_else(|| panic!("{label}: no record after a checkpoint"));
        let claimed = assert_claims_are_exact(&observed, &part, &pattern(1, size));
        assert!(claimed > 0 && claimed < size, "{label}: {claimed} bytes");
    }
}

fn every_stop_leaves_a_record_of_true_bytes(connections: u8, size: u64, rate: u64, step: u64) {
    let server = Server::start(Config {
        size,
        throttle: Some(rate),
        ..Config::default()
    });
    checkpoint_every(NEVER, MIB);
    let request = request_for(&server);
    let expected = pattern(1, size);
    let mut interrupted = 0;
    for stop in 0..20u64 {
        let label = format!("stop {stop}");
        let directory = TempDir::new("stop-points");
        let output = directory.0.join("payload.bin");
        let control = TransferControl::default();
        let watcher = stop_after(&control, Duration::from_millis(10 + stop * step));
        let mut reported = 0;
        let first = fetch(
            &request,
            &output,
            &control,
            connections,
            connections,
            &mut |progress| reported = reported.max(progress.downloaded),
        );
        watcher.join().unwrap();
        server.wait_idle();
        if let Ok(result) = first {
            assert_eq!(
                file_sha256(&result.path),
                pattern_sha256(1, size),
                "{label}"
            );
            continue;
        }
        interrupted += 1;
        let claimed = claimed_bytes(&output, &expected);
        assert_eq!(retained_bytes(&output), Some(claimed), "{label}");
        // Whatever the transfer had received when it stopped is recorded.
        assert!(
            claimed >= reported,
            "{label}: {claimed} recorded, {reported} reported"
        );
        let recorded = read_state(&state_path(&output))
            .filter(|state| state.segmented)
            .map(|state| {
                state
                    .completed
                    .iter()
                    .map(|range| (range.start, range.end))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let first_attempt = server.log().len();
        forget_writes();
        let result = fetch(
            &request,
            &output,
            &TransferControl::default(),
            connections,
            connections,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{label}: {error:#}"));
        assert_eq!(
            file_sha256(&result.path),
            pattern_sha256(1, size),
            "{label}"
        );
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        server.wait_idle();
        let log = server.log();
        let resumed = &log[first_attempt..];
        if connections > 1 {
            assert_written_once(&writes(), &recorded, size);
            let mut requested = delivered(resumed, size);
            requested.extend(recorded);
            assert_tiles(requested, size);
        } else if claimed > 0 && claimed < size {
            let gets = data_gets(resumed).collect::<Vec<_>>();
            assert_eq!(gets.len(), 1, "{label}: {resumed:?}");
            assert_eq!(gets[0].range, Some(format!("bytes={claimed}-")), "{label}");
        }
    }
    assert!(interrupted > 0, "no stop interrupted the download");
}

#[test]
fn stopping_a_segmented_download_at_any_point_leaves_a_record_of_true_bytes() {
    every_stop_leaves_a_record_of_true_bytes(2, 24 * MIB + 4_097, 32 * MIB, 25);
}

#[test]
fn stopping_a_one_connection_download_at_any_point_leaves_a_record_of_true_bytes() {
    every_stop_leaves_a_record_of_true_bytes(1, 12 * MIB + 17, 32 * MIB, 18);
}

/// Opens `path` the way a scanner or a sync client can: other processes may read it but not
/// replace or delete it. Retries while a record write has the file.
fn hold_against_replacement(path: &Path) -> File {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    for _ in 0..100 {
        if let Ok(file) = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
        {
            return file;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{} could not be held", path.display());
}

/// The record cannot be replaced for as long as the first periodic cycle takes to give up, then
/// it can again; the download has to finish all the same.
fn survives_periodic_checkpoints_that_fail(connections: u8, size: u64) {
    let server = Server::start(Config {
        size,
        throttle: Some(2 * MIB),
        ..Config::default()
    });
    checkpoint_every(Duration::from_secs(1), u64::MAX);
    let directory = TempDir::new("failing-checkpoint");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);
    let mut held = None;
    let mut acquired = false;
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |progress| {
            if !acquired && progress.downloaded > 0 {
                held = Some(hold_against_replacement(&state_file));
                acquired = true;
            }
            if failed_cycles() > 0 {
                held = None;
            }
        },
    );
    drop(held);
    let result = result.unwrap_or_else(|error| {
        panic!(
            "{connections} connection(s): {error:#}; steps {:?}",
            steps()
        )
    });
    assert!(failed_cycles() > 0, "no periodic cycle failed");
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
}

#[test]
fn a_one_connection_download_survives_periodic_checkpoints_that_fail() {
    survives_periodic_checkpoints_that_fail(1, 12 * MIB);
}

#[test]
fn a_segmented_download_survives_periodic_checkpoints_that_fail() {
    survives_periodic_checkpoints_that_fail(2, 24 * MIB);
}

/// Runs a one-connection download while the record cannot be replaced from the first byte until
/// `fetch` returns, and no periodic cycle runs: only the cycle at the end could write it.
fn fetch_with_a_record_that_cannot_be_written(
    config: Config,
) -> (TempDir, PathBuf, Result<TransferResult>) {
    let server = Server::start(config);
    checkpoint_every(NEVER, u64::MAX);
    let directory = TempDir::new("final-record");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);
    let mut held = None;
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |progress| {
            if held.is_none() && progress.downloaded > 0 {
                held = Some(hold_against_replacement(&state_file));
            }
        },
    );
    assert!(held.is_some(), "the record was never held");
    drop(held);
    (directory, output, result)
}

#[test]
fn a_one_connection_download_completes_when_only_the_final_record_cannot_be_written() {
    let size = 6 * MIB;
    let (_directory, output, result) = fetch_with_a_record_that_cannot_be_written(Config {
        size,
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    let result = result.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists());
}

#[test]
fn a_transport_error_stays_a_transport_error_when_the_record_cannot_be_written() {
    let size = 6 * MIB;
    let cut = 3 * MIB;
    let (_directory, output, result) = fetch_with_a_record_that_cannot_be_written(Config {
        size,
        throttle: Some(4 * MIB),
        cut_body: Some(CutBody {
            after_bytes: cut,
            responses: 1,
        }),
        ..Config::default()
    });
    let error = result.expect_err("the body was cut");
    // The engine retries what carries a libcurl transport error and not a disk error.
    let chain = error.chain().collect::<Vec<_>>();
    assert!(
        chain
            .iter()
            .any(|cause| cause.downcast_ref::<curl::Error>().is_some()),
        "{error:#}"
    );
    assert!(
        !chain
            .iter()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some()),
        "{error:#}"
    );
    assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), cut);
}

fn flushes_before_it_records(connections: u8) {
    let size = 12 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    checkpoint_every(Duration::from_millis(100), u64::MAX);
    let directory = TempDir::new("cycle-order");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    // Every cycle reads the counts first, flushes the part file through its own handle and
    // through every writer handle, and records last, and none overlaps another: the periodic
    // ones and the one at the end. The end of a one-connection download is flushed and not
    // recorded, because the record is deleted when the file is published.
    let cycles = cycles();
    assert!(cycles.len() >= 3, "{:?}", steps());
    for (index, cycle) in cycles.iter().enumerate() {
        let last = index + 1 == cycles.len();
        assert_eq!(
            *cycle,
            cycle_of(connections, connections > 1 || !last),
            "{:?}",
            steps()
        );
    }
}

#[test]
fn a_one_connection_download_flushes_before_it_records() {
    flushes_before_it_records(1);
}

#[test]
fn a_segmented_download_flushes_before_it_records() {
    flushes_before_it_records(2);
}

fn the_end_waits_for_the_running_cycle(connections: u8) {
    let size = 4 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    // The first periodic cycle starts early and then holds for two seconds, which is longer
    // than the transfer: it is still running when the last bytes arrive.
    checkpoint_every(Duration::from_millis(100), u64::MAX);
    pause_cycles_for(Duration::from_secs(2));
    let directory = TempDir::new("cycle-running");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    // The cycle at the end starts only after the running one has recorded; no second periodic
    // cycle starts while one is running.
    assert_eq!(
        cycles(),
        [
            cycle_of(connections, true),
            cycle_of(connections, connections > 1)
        ],
        "{:?}",
        steps()
    );
}

#[test]
fn the_end_of_a_one_connection_download_waits_for_the_running_cycle() {
    the_end_waits_for_the_running_cycle(1);
}

#[test]
fn the_end_of_a_segmented_download_waits_for_the_running_cycle() {
    the_end_waits_for_the_running_cycle(2);
}

#[test]
fn the_time_trigger_fires_while_no_lease_finishes() {
    let lease = 8 * MIB;
    let size = 2 * lease;
    let server = Server::start(Config {
        size,
        throttle: Some(2 * MIB),
        ..Config::default()
    });
    checkpoint_every(Duration::from_millis(50), u64::MAX);
    let directory = TempDir::new("time-trigger");
    let output = directory.0.join("payload.bin");
    let control = TransferControl::default();
    let cancel = control.clone();
    let state_file = state_path(&output);
    let mut seen = None;
    let result = fetch(
        &request_for(&server),
        &output,
        &control,
        2,
        2,
        &mut |progress| {
            // Both leases are 8 MiB, so neither can have finished before 6 MiB are written in
            // all; only the clock can have started a cycle until then.
            if seen.is_none() && (checkpoint_cycles() > 0 || progress.downloaded >= 6 * MIB) {
                seen = Some((
                    checkpoint_cycles(),
                    read_state(&state_file).zip(fs::read(part_path(&output)).ok()),
                ));
                cancel.cancel();
            }
        },
    );
    assert!(result.is_err());
    let (cycles, record) = seen.expect("the download was never observed");
    assert!(cycles > 0, "no cycle started before a lease could finish");
    let (record, part) = record.expect("the cycle left no record");
    let claimed = assert_claims_are_exact(&record, &part, &pattern(1, size));
    assert!(claimed > 0 && claimed < lease, "{claimed} bytes");
}

#[test]
fn a_part_file_longer_than_its_recorded_length_is_cut_back_on_resume() {
    let size = 3 * MIB + 17;
    let server = Server::start(Config {
        size,
        throttle: Some(2 * MIB),
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("longer-part");
    let output = directory.0.join("payload.bin");

    let control = TransferControl::default();
    let cancel = control.clone();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    let synced = read_state(&state_path(&output)).unwrap().synced.unwrap();
    assert!(synced > 0 && synced < size);
    assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), synced);
    // What a power loss leaves behind when the file length reached the disk before the data;
    // the junk reaches past the end of the file.
    let mut part = OpenOptions::new()
        .append(true)
        .open(part_path(&output))
        .unwrap();
    part.write_all(&vec![0xAA; size as usize]).unwrap();
    drop(part);
    assert_eq!(retained_bytes(&output), Some(synced));
    server.wait_idle();
    let first_attempt = server.log().len();

    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    let log = server.log();
    let resumed = log[first_attempt..]
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(resumed.len(), 1, "{log:?}");
    assert_eq!(resumed[0].range, Some(format!("bytes={synced}-")));
    assert_eq!(resumed[0].status, 206);
}

/// The text of a resume record as the release before this one writes it: no `synced` field.
fn previous_release_state(server: &Server, size: u64, segmented: bool, completed: &str) -> String {
    let digest = source_url_digest(&Url::parse(&server.url("/payload.bin")).unwrap());
    format!(
        r#"{{"version":3,"url":"{digest}","etag":"\"v1\"","last_modified":"Mon, 01 Jan 2024 00:00:00 GMT","total":{size},"segmented":{segmented},"connections":{},"checksum":null,"completed":{completed}}}"#,
        if segmented { 2 } else { 1 }
    )
}

#[test]
fn a_one_connection_record_of_the_previous_release_resumes_from_the_length_of_the_part_file() {
    let size = 3 * MIB + 17;
    let held = 1_000_003usize;
    let server = Server::start(Config {
        size,
        ..Config::default()
    });
    let directory = TempDir::new("previous-single");
    let output = directory.0.join("payload.bin");
    fs::write(part_path(&output), &pattern(1, size)[..held]).unwrap();
    fs::write(
        state_path(&output),
        previous_release_state(&server, size, false, "[]"),
    )
    .unwrap();
    assert_eq!(read_state(&state_path(&output)).unwrap().synced, None);
    assert_eq!(retained_bytes(&output), Some(held as u64));

    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    let log = server.log();
    let gets = log
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(gets.len(), 1, "{log:?}");
    assert_eq!(gets[0].range, Some(format!("bytes={held}-")));
    assert_eq!(gets[0].if_range.as_deref(), Some("\"v1\""));
    assert_eq!(gets[0].status, 206);
}

#[test]
fn a_segmented_record_of_the_previous_release_resumes_with_only_the_missing_ranges() {
    let size = 24 * MIB + 12_345;
    let done = 8 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(64 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("previous-segmented");
    let output = directory.0.join("payload.bin");
    let mut part = File::create(part_path(&output)).unwrap();
    part.write_all(&pattern(1, size)[..done as usize]).unwrap();
    part.set_len(size).unwrap();
    drop(part);
    fs::write(
        state_path(&output),
        previous_release_state(
            &server,
            size,
            true,
            &format!(r#"[{{"start":0,"end":{done}}}]"#),
        ),
    )
    .unwrap();
    assert_eq!(retained_bytes(&output), Some(done));

    forget_writes();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert_written_once(&writes(), &[(0, done)], size);
    server.wait_idle();
    let mut requested = delivered(&server.log(), size);
    requested.push((0, done));
    assert_tiles(requested, size);
}

#[test]
fn a_record_keeps_the_format_of_the_previous_release_except_for_the_synced_length() {
    let segmented = ResumeState {
        completed: vec![ByteRange { start: 0, end: 5 }],
        segmented: true,
        ..ResumeState::default()
    };
    let text = String::from_utf8(serde_json::to_vec(&segmented).unwrap()).unwrap();
    assert!(!text.contains("synced"), "{text}");
    let single = ResumeState {
        synced: Some(7),
        ..ResumeState::default()
    };
    let text = String::from_utf8(serde_json::to_vec(&single).unwrap()).unwrap();
    assert!(text.ends_with(r#""completed":[],"synced":7}"#), "{text}");
}

#[test]
fn a_worker_with_nothing_left_takes_the_back_of_the_lease_that_has_stalled() {
    let size = 12 * MIB + 4_097;
    // Two leases of about 6 MiB. The connection of the second one stops after 1 MiB until
    // another request takes part of its range, and the throttle keeps the first lease running
    // until the second one has been requested.
    let run = download_with_a_stall(size, 2, MIB, Some(24 * MIB));
    run.assert_exact(&pattern(1, size));

    let lease = size.div_ceil(2);
    let found = steals(&run.log, size);
    assert_eq!(found.len(), 1, "{found:?}");
    let steal = &found[0];
    assert!(steal.victim_first, "{steal:?}");
    assert_eq!(steal.victim, (lease, size));
    assert_eq!(steal.thief.1, size);
    // The lease keeps the front half of what was to come, of which 1 MiB at most had arrived.
    let length = size - lease;
    let kept = steal.thief.0 - lease;
    assert!(
        kept >= length.div_ceil(2) && kept <= (length + MIB).div_ceil(2),
        "{steal:?} keeps {kept} of {length} bytes"
    );
    assert_eq!(ranged_gets(&run.log, size).len(), 3, "{:?}", run.log);
}

#[test]
fn the_worker_of_a_cut_lease_writes_up_to_the_cut_and_ends_it_as_complete() {
    let size = 12 * MIB + 4_097;
    let run = download_with_a_stall(size, 2, MIB, Some(24 * MIB));
    run.assert_exact(&pattern(1, size));
    let lease = size.div_ceil(2);
    let found = steals(&run.log, size);
    assert_eq!(found.len(), 1, "{found:?}");
    let cut = found[0].thief.0;

    // The worker wrote every byte of its lease up to the cut, and its last write ends there.
    let own = run
        .writes
        .iter()
        .copied()
        .filter(|&(offset, _)| (lease..cut).contains(&offset))
        .collect::<Vec<_>>();
    assert!(
        own.iter().all(|&(offset, length)| offset + length <= cut),
        "a write crosses the cut {cut}: {own:?}"
    );
    assert_eq!(
        own.iter().map(|&(_, length)| length).sum::<u64>(),
        cut - lease
    );
    // It ended without an error: the download returned, the progress carries no notice of a
    // retry and nothing was asked for again. A lease that ends at its cut is a completed lease,
    // not a failed attempt.
    assert_eq!(ranged_gets(&run.log, size).len(), 3, "{:?}", run.log);
}

#[test]
fn a_lease_with_exactly_two_minimum_pieces_to_come_is_cut_into_two() {
    // Two leases of 4 MiB and 1 KiB. The second stalls after its first KiB, which is all that
    // tells its rate, so it has 4 MiB to come: the least that is cut, into two pieces of
    // `MIN_SEGMENT_SIZE`.
    let size = 8 * MIB + 2_048;
    let lease = size / 2;
    let run = download_with_a_stall(size, 2, 1_024, Some(24 * MIB));
    run.assert_exact(&pattern(1, size));
    let found = steals(&run.log, size);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].victim, (lease, size));
    assert_eq!(found[0].thief, (lease + 1_024 + 2 * MIB, size));
}

#[test]
fn a_worker_that_has_finished_takes_the_back_of_a_lease_that_is_slow() {
    // Two leases of 8 MiB. The second comes over a connection at 4 MiB/s, the first ends after
    // an eighth of a second at 64 MiB/s: its worker fetches the back half of what is left of the
    // second far sooner than that lease would end.
    let size = 16 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(64 * MIB),
        slow_connection: Some((0, 4 * MIB)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("slow-lease");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_file_is(&result.path, &pattern(1, size), "a slow lease");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    assert_tiles(delivered(&log, size), size);
    let found = steals(&log, size);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].victim, (8 * MIB, size));
    assert_eq!(ranged_gets(&log, size).len(), 3, "{log:?}");
}

#[test]
fn a_slow_connection_with_nothing_left_does_not_take_the_back_of_a_fast_lease() {
    // The record leaves the first 8 MiB and the last 64 KiB. The 64 KiB come over a connection
    // at 0.25 MiB/s and end after a quarter of a second, when the lease of the first 8 MiB, at
    // 8 MiB/s, has 6 MiB to come. The slow connection would need 12 s for the back half of
    // them, the lease needs 0.75 s for all.
    let size = 16 * MIB;
    let tail = size - 64 * 1024;
    let source = pattern(1, size);
    let server = Server::start(Config {
        size,
        throttle: Some(8 * MIB),
        slow_connection: Some((0, MIB / 4)),
        ..Config::default()
    });
    let directory = TempDir::new("slow-thief");
    let output = directory.0.join("payload.bin");
    let part = File::create(part_path(&output)).unwrap();
    part.set_len(size).unwrap();
    write_all_at(&part, &source[(8 * MIB) as usize..tail as usize], 8 * MIB).unwrap();
    drop(part);
    fs::write(
        state_path(&output),
        previous_release_state(
            &server,
            size,
            true,
            &format!(r#"[{{"start":{},"end":{tail}}}]"#, 8 * MIB),
        ),
    )
    .unwrap();

    forget_writes();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_file_is(&result.path, &source, "a slow thief");
    assert_written_once(&writes(), &[(8 * MIB, tail)], size);

    server.wait_idle();
    let log = server.log();
    let mut ranges = ranged_gets(&log, size);
    ranges.sort_unstable();
    assert_eq!(ranges, [(0, 8 * MIB), (tail, size)], "{log:?}");
}

#[test]
fn a_lease_with_less_to_come_than_two_minimum_pieces_is_left_to_its_worker() {
    // Two leases of 4 MiB less one byte. The second is slow, so the worker of the first is idle
    // for a second, while the second has one byte less to come than the least that is cut.
    let size = 8 * MIB - 2;
    let lease = size / 2;
    let server = Server::start(Config {
        size,
        slow_connection: Some((0, 4 * MIB)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("below-threshold");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap();
    assert_file_is(&result.path, &pattern(1, size), "below the threshold");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let mut ranges = ranged_gets(&server.log(), size);
    ranges.sort_unstable();
    assert_eq!(ranges, [(0, lease), (lease, size)]);
}

#[test]
fn leases_cut_at_varied_moments_are_written_exactly_once() {
    let mut random = Sequence::new(0x5851_F42D_4C95_7F2D);
    let source = pattern(1, 4 * 8 * MIB + 4_096);
    let mut cuts = std::collections::BTreeSet::new();
    for run in 0..24u64 {
        let connections = 2 + (run % 3) as u8;
        // Leases of 8 MiB, one of which stalls after at least one byte, which tells its rate, and
        // at most 3 MiB: it keeps at least 5 MiB to come, so that it is cut however many bytes
        // arrived before it stalled.
        let size = u64::from(connections) * 8 * MIB + random.below(4_096);
        let after = 1 + random.below(3 * MIB);
        let throttle = [None, Some(64 * MIB), Some(160 * MIB)][random.below(3) as usize];
        // Every other run checkpoints so often that cycles run while the leases are cut.
        if run % 2 == 0 {
            checkpoint_every(Duration::from_millis(5), 256 * 1024);
        } else {
            checkpoint_every(NEVER, u64::MAX);
        }
        let stalled = download_with_a_stall(size, connections, after, throttle);
        stalled.assert_exact(&source[..size as usize]);
        let found = steals(&stalled.log, size);
        assert!(
            !found.is_empty(),
            "no lease was cut: {:?}",
            ranged_gets(&stalled.log, size)
        );
        cuts.extend(found.iter().map(|steal| steal.thief.0 - steal.victim.0));
    }
    assert!(cuts.len() >= 12, "{} different cuts: {cuts:?}", cuts.len());
}

/// The ranges the record of `output` holds.
fn recorded_ranges(output: &Path) -> Vec<(u64, u64)> {
    read_state(&state_path(output))
        .filter(|state| state.segmented)
        .map(|state| {
            state
                .completed
                .iter()
                .map(|range| (range.start, range.end))
                .collect()
        })
        .unwrap_or_default()
}

/// Resumes the download of `output` from `recorded` and checks that it asks for and writes
/// exactly the bytes that no record holds.
fn resume_exactly(
    server: &Server,
    output: &Path,
    recorded: &[(u64, u64)],
    expected: &[u8],
    label: &str,
) {
    let size = expected.len() as u64;
    server.wait_idle();
    let first_attempt = server.log().len();
    forget_writes();
    let result = fetch(
        &request_for(server),
        output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap_or_else(|error| panic!("{label}: {error:#}"));
    assert_file_is(&result.path, expected, label);
    assert!(!part_path(output).exists() && !state_path(output).exists());
    server.wait_idle();
    let mut requested = delivered(&server.log()[first_attempt..], size);
    requested.extend_from_slice(recorded);
    assert_tiles(requested, size);
    assert_written_once(&writes(), recorded, size);
}

/// Downloads `size` bytes over two connections from `server` and stops the download once
/// `wait` returns. `wait` runs on another thread as soon as a lease has been cut, and gets the
/// offset of the cut and whether the download has ended. Returns the outcome of the download and
/// what `wait` returned, which is nothing when the download ended before the cut.
fn download_stopped_after_a_cut<T: Send>(
    server: &Server,
    output: &Path,
    size: u64,
    wait: impl FnOnce(u64, &AtomicBool) -> T + Send,
) -> (Result<TransferResult>, Option<T>) {
    let control = TransferControl::default();
    let finished = AtomicBool::new(false);
    forget_writes();
    thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let cut = loop {
                if let Some(steal) = steals(&server.log(), size).first() {
                    break steal.thief.0;
                }
                if finished.load(Ordering::Acquire) {
                    return None;
                }
                thread::sleep(Duration::from_millis(1));
            };
            let seen = wait(cut, &finished);
            control.cancel();
            Some(seen)
        });
        let first = fetch(&request_for(server), output, &control, 2, 2, &mut |_| {});
        finished.store(true, Ordering::Release);
        (first, watcher.join().unwrap())
    })
}

/// Checks what a download that was stopped after a cut left behind, or its file if it finished
/// first, and that resuming it asks for and writes exactly the bytes that are missing.
fn assert_stop_is_exact(
    server: &Server,
    output: &Path,
    first: Result<TransferResult>,
    expected: &[u8],
    label: &str,
) {
    let size = expected.len() as u64;
    server.wait_idle();
    assert!(
        !steals(&server.log(), size).is_empty(),
        "{label}: no lease was cut"
    );
    assert_no_byte_written_twice(&writes());
    match first {
        Ok(result) => {
            assert_file_is(&result.path, expected, label);
            assert_written_once(&writes(), &[], size);
        }
        Err(_) => {
            let claimed = claimed_bytes(output, expected);
            assert!(claimed > 0, "{label}: nothing was recorded");
            assert_eq!(retained_bytes(output), Some(claimed), "{label}");
            let recorded = recorded_ranges(output);
            resume_exactly(server, output, &recorded, expected, label);
        }
    }
}

#[test]
fn a_download_stopped_right_after_a_cut_records_true_bytes_and_resumes_exactly() {
    let size = 16 * MIB + 4_097;
    let expected = pattern(1, size);
    // No cycle runs but the one that the stop starts.
    checkpoint_every(NEVER, u64::MAX);
    for delay in [0u64, 1, 2, 4, 8, 16, 32, 48] {
        let label = format!("stopped {delay} ms after the request for the back of a lease");
        // Two leases of 8 MiB; the second stalls after 1 MiB until the back of it is requested,
        // and the throttle gives the stop the time of the two transfers that follow.
        let server = Server::start(Config {
            size,
            throttle: Some(64 * MIB),
            stall: Some((0, MIB)),
            ..Config::default()
        });
        let directory = TempDir::new("stop-after-cut");
        let output = directory.0.join("payload.bin");
        let (first, _) = download_stopped_after_a_cut(&server, &output, size, |_, _| {
            thread::sleep(Duration::from_millis(delay))
        });
        assert_stop_is_exact(&server, &output, first, &expected, &label);
    }
}

#[test]
fn a_record_written_while_a_lease_is_cut_claims_only_flushed_bytes_and_resumes_exactly() {
    let size = 16 * MIB + 4_097;
    let expected = pattern(1, size);
    // The lease that is cut goes on at 1 MiB/s, so the download lasts until the scheduler has
    // started a cycle that records what the other connection fetched.
    let server = Server::start(Config {
        size,
        throttle: Some(64 * MIB),
        slow_connection: Some((0, MIB)),
        stall: Some((0, MIB / 4)),
        ..Config::default()
    });
    checkpoint_every(Duration::from_millis(20), 64 * 1024);
    let directory = TempDir::new("record-after-cut");
    let output = directory.0.join("payload.bin");
    let (first, image) = download_stopped_after_a_cut(&server, &output, size, |cut, finished| {
        // The first record that holds bytes of the range that was cut off, read as a crash
        // would find it: the record, then the part file.
        loop {
            if let Some(state) = read_state(&state_path(&output)) {
                if state.completed.iter().any(|range| range.end > cut) {
                    return fs::read(part_path(&output)).ok().map(|part| (state, part));
                }
            }
            if finished.load(Ordering::Acquire) {
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let (state, part) = image
        .flatten()
        .expect("no record held bytes of the back of the cut lease");
    assert_claims_are_exact(&state, &part, &expected);
    assert_stop_is_exact(&server, &output, first, &expected, "the stop");

    let copy = TempDir::new("crash-image");
    let image_output = copy.0.join("payload.bin");
    fs::write(part_path(&image_output), &part).unwrap();
    write_state(&state_path(&image_output), &state).unwrap();
    let recorded = recorded_ranges(&image_output);
    resume_exactly(&server, &image_output, &recorded, &expected, "the image");
}

#[test]
fn a_source_that_changes_before_the_back_of_a_lease_is_requested_is_never_mixed_into_the_file() {
    let size = 12 * MIB + 4_097;
    for ignore_if_range in [false, true] {
        let label = format!("the server ignores If-Range: {ignore_if_range}");
        // The HEAD (request 0), the probe (1) and the two leases (2 and 3) see version 1. The
        // request for the back of the stalled lease is the fifth and sees version 2.
        let server = Server::start(Config {
            size,
            throttle: Some(24 * MIB),
            stall: Some((0, MIB)),
            switch_version_after: Some(4),
            ignore_if_range,
            ..Config::default()
        });
        checkpoint_every(NEVER, u64::MAX);
        let directory = TempDir::new("changed-at-cut");
        let output = directory.0.join("payload.bin");
        let result = fetch(
            &request_for(&server),
            &output,
            &TransferControl::default(),
            2,
            2,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{label}: {error:#}"));
        assert_file_is(&result.path, &pattern(2, size), &label);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());

        server.wait_idle();
        let log = server.log();
        let found = steals(&log, size);
        assert_eq!(found.len(), 1, "{label}: {found:?}");
        let thief = log
            .iter()
            .position(|entry| {
                entry.range == Some(format!("bytes={}-{}", found[0].thief.0, size - 1))
            })
            .unwrap();
        assert_eq!(thief, 4, "{label}: {log:?}");
        // The request for the back of the lease carries the validator of the record, and the
        // answer of another version is not taken.
        assert_eq!(log[thief].if_range.as_deref(), Some("\"v1\""), "{label}");
        assert_eq!(
            log[thief].status,
            if ignore_if_range { 206 } else { 200 },
            "{label}"
        );
        // The bytes of version 1 are discarded and the file comes whole from one request.
        let last = log.last().unwrap();
        assert_eq!((last.method.as_str(), last.range.as_deref()), ("GET", None));
    }
}

/// What a download left behind on a server whose second lease stalls after 1 MiB until the back
/// of it is requested, and whose first `responses` responses that start at or after `cut_from`
/// drop after `after` bytes. The download has to finish with the exact file all the same.
struct Dropped {
    log: Vec<Logged>,
    phases: Vec<String>,
}

fn download_with_a_stall_and_drops(
    size: u64,
    after: u64,
    responses: usize,
    cut_from: u64,
) -> Dropped {
    let server = Server::start(Config {
        size,
        throttle: Some(24 * MIB),
        stall: Some((0, MIB)),
        cut_body: Some(CutBody {
            after_bytes: after,
            responses,
        }),
        cut_from,
        ..Config::default()
    });
    checkpoint_every(NEVER, u64::MAX);
    forget_writes();
    let directory = TempDir::new("steal-and-drop");
    let output = directory.0.join("payload.bin");
    let mut phases = Vec::new();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |progress| phases.push(progress.phase),
    )
    .unwrap_or_else(|error| panic!("{error:#}"));
    assert_file_is(&result.path, &pattern(1, size), "steal and drop");
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    assert_written_once(&writes(), &[], size);
    server.wait_idle();
    let log = server.log();
    assert_tiles(delivered(&log, size), size);
    for entry in data_gets(&log) {
        assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
    }
    Dropped { log, phases }
}

/// Where the back of the lease that starts at `lease` was first requested: the lowest start
/// above `lease` among the requests that end at the end of the file.
fn cut_of(log: &[Logged], size: u64, lease: u64) -> u64 {
    ranged_gets(log, size)
        .into_iter()
        .filter(|&(from, to)| to == size && from > lease)
        .map(|(from, _)| from)
        .min()
        .expect("no lease was cut")
}

#[test]
fn a_victim_that_drops_before_its_cut_is_asked_for_again_only_up_to_the_cut() {
    let size = 12 * MIB + 4_097;
    let lease = size.div_ceil(2);
    // The response of the stalled lease drops after 2 MiB, before its cut at about 3.5 MiB.
    let run = download_with_a_stall_and_drops(size, 2 * MIB, 1, lease);
    let cut = cut_of(&run.log, size, lease);
    let ranges = ranged_gets(&run.log, size);
    assert!(ranges.contains(&(lease + 2 * MIB, cut)), "{ranges:?}");
    assert!(
        run.phases.iter().any(|phase| is_retry_notice(phase)),
        "{:?}",
        run.phases
    );
}

#[test]
fn a_thief_that_drops_is_asked_for_again_from_where_it_stopped() {
    let size = 12 * MIB + 4_097;
    let lease = size.div_ceil(2);
    // The first response that starts above the stalled lease is the one for its back; it drops
    // after 1 MiB.
    let run = download_with_a_stall_and_drops(size, MIB, 1, lease + 1);
    let cut = cut_of(&run.log, size, lease);
    let ranges = ranged_gets(&run.log, size);
    assert!(ranges.contains(&(cut + MIB, size)), "{ranges:?}");
    assert!(
        run.phases.iter().any(|phase| is_retry_notice(phase)),
        "{:?}",
        run.phases
    );
}

#[test]
fn a_victim_and_a_thief_that_both_drop_are_each_asked_for_again_from_where_they_stopped() {
    let size = 12 * MIB + 4_097;
    let lease = size.div_ceil(2);
    let run = download_with_a_stall_and_drops(size, MIB + MIB / 2, 2, lease);
    let cut = cut_of(&run.log, size, lease);
    let ranges = ranged_gets(&run.log, size);
    assert!(
        ranges.contains(&(lease + MIB + MIB / 2, cut)),
        "the victim: {ranges:?}"
    );
    assert!(
        ranges.contains(&(cut + MIB + MIB / 2, size)),
        "the thief: {ranges:?}"
    );
}

#[test]
fn a_download_stopped_after_a_cut_whose_victim_dropped_records_true_bytes_and_resumes_exactly() {
    let size = 12 * MIB + 4_097;
    let lease = size.div_ceil(2);
    let expected = pattern(1, size);
    checkpoint_every(NEVER, u64::MAX);
    // The stops fall before the drop, between the drop and the retry, and after the retry.
    for delay in [0u64, 20, 60, 150, 600, 1300] {
        let label = format!("stopped {delay} ms after the request for the back of a lease");
        let server = Server::start(Config {
            size,
            throttle: Some(24 * MIB),
            stall: Some((0, MIB)),
            cut_body: Some(CutBody {
                after_bytes: 2 * MIB,
                responses: 1,
            }),
            cut_from: lease,
            ..Config::default()
        });
        let directory = TempDir::new("stop-after-drop");
        let output = directory.0.join("payload.bin");
        let (first, _) = download_stopped_after_a_cut(&server, &output, size, |_, _| {
            thread::sleep(Duration::from_millis(delay))
        });
        assert_stop_is_exact(&server, &output, first, &expected, &label);
    }
}

const BENCH_SIZE: u64 = 128 * MIB;
const BENCH_RATE: u64 = 4 * MIB;
/// The first connection that starts with a ranged GET above byte 0, so the slow socket does
/// not depend on how many probe connections come first.
const SLOW_CONNECTION: usize = 0;

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
    let sockets = data_gets(&log)
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
    bench(
        "8 connections and per-host limit 8, 4 MiB/s each",
        throttled(),
        8,
        8,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_one_slow_connection() {
    bench(
        "8 connections and per-host limit 8, one at 0.5 MiB/s",
        Config {
            slow_connection: Some((SLOW_CONNECTION, MIB / 2)),
            ..throttled()
        },
        8,
        8,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_dropped_connections() {
    bench(
        "8 connections and per-host limit 8, first 6 responses longer than 4 MiB cut after 4 MiB",
        Config {
            cut_body: Some(CutBody {
                after_bytes: 4 * MIB,
                responses: 6,
            }),
            ..throttled()
        },
        8,
        8,
    );
}

#[test]
#[ignore = "benchmark: cargo test --lib bench_ -- --ignored --nocapture --test-threads=1"]
fn bench_ranges_without_accept_ranges_header() {
    bench(
        "8 connections and per-host limit 8, no Accept-Ranges header",
        Config {
            accept_ranges_header: false,
            ..throttled()
        },
        8,
        8,
    );
}
