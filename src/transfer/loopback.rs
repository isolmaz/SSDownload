//! Behaviour pins and a local benchmark that drive `download` against the configurable
//! loopback server in `crate::test_http`.

use super::*;
use crate::test_http::{
    file_sha256, pattern, pattern_sha256, repr_digest_header, Config, CutBody, Document, EtagKind,
    Logged, RangeSupport, Redirect, RetryAfter, Scripted, Sequence, Server, Signing, TempDir,
    Validity,
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
fn a_one_connection_download_whose_answer_brings_the_identity_reports_what_the_next_attempt_does() {
    // The HEAD names no identity; the GET's ETag is what the state keeps.
    let size = MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(MIB),
        head_bare: true,
        ..Config::default()
    });
    let request = request_for(&server);
    let directory = TempDir::new("adopted-identity");
    let output = directory.0.join("payload.bin");
    let control = TransferControl::default();
    let cancel = control.clone();
    let mut first = Vec::new();
    let interrupted = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        first.push(progress.resumable);
        if progress.downloaded > 0 {
            cancel.cancel();
        }
    });
    assert!(interrupted.is_err());
    assert!(!first.is_empty());
    // Nothing claims that the next attempt starts over: one connection asked no range, so
    // whether the source answers ranges is unknown.
    assert!(first.iter().all(Option::is_none), "{first:?}");

    let mut second = Vec::new();
    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |progress| second.push(progress.resumable),
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    server.wait_idle();
    assert!(
        server
            .log()
            .iter()
            .any(|entry| entry.method == "GET" && entry.status == 206),
        "the next attempt resumed"
    );
    assert!(!second.is_empty());
    assert!(
        second.iter().all(|value| *value == Some(true)),
        "{second:?}"
    );
}

#[test]
fn a_segmented_download_that_falls_back_to_one_connection_reports_that_its_bytes_are_not_kept() {
    // Every answer carries its own ETag: the ranges cannot be joined, the download goes on
    // over one connection, and a later attempt would not find the same source again.
    let size = 12 * MIB + 7;
    let server = Server::start(Config {
        size,
        vary_etag: true,
        throttle: Some(16 * MIB),
        ..Config::default()
    });
    let directory = TempDir::new("varying-etag-fallback");
    let output = directory.0.join("payload.bin");
    let mut events = Vec::new();
    let result = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |progress| events.push(progress),
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    let single = events
        .iter()
        .rposition(|event| event.ranges.is_some())
        .map_or(&events[..], |at| &events[at + 1..]);
    assert!(!single.is_empty(), "no report over one connection");
    for event in single {
        assert_eq!(event.resumable, Some(false), "{event:?}");
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

/// Makes the range workers of the downloads that the current test thread starts wait `delay`
/// between receiving a range and requesting it, as a busy machine makes them.
fn delay_range_starts_by(delay: Duration) {
    TEST_START_DELAY.with(|current| current.set(delay));
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

/// The rate at which a test holds a download while it waits for checkpoint cycles. The 12 MiB of
/// the smallest of these downloads take more than three minutes at it, so the download ends long
/// after the cycles that the test waits for.
const HELD_RATE: u64 = 64 * 1024;

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
/// it can again; the download has to finish all the same. The download is held at `HELD_RATE`
/// until that cycle has failed and the record is free: a segmented download that ended first
/// would have its last record refused as well.
fn survives_periodic_checkpoints_that_fail(connections: u8, size: u64) {
    let server = Server::start(Config {
        size,
        ..Config::default()
    });
    checkpoint_every(Duration::from_secs(1), u64::MAX);
    let directory = TempDir::new("failing-checkpoint");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);
    let control = TransferControl::default();
    control.set_speed_limit(HELD_RATE);
    let mut held = None;
    let mut acquired = false;
    let result = fetch(
        &request_for(&server),
        &output,
        &control,
        connections,
        connections,
        &mut |progress| {
            if !acquired && progress.downloaded > 0 {
                held = Some(hold_against_replacement(&state_file));
                acquired = true;
            }
            if failed_cycles() > 0 {
                held = None;
                control.set_speed_limit(0);
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

/// Runs a download over `connections` connections while the record cannot be replaced from the
/// first byte until `fetch` returns, and no periodic cycle runs: only the cycle at the end could
/// write it.
fn fetch_with_a_record_that_cannot_be_written(
    connections: u8,
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
        connections,
        connections,
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
    let (_directory, output, result) = fetch_with_a_record_that_cannot_be_written(
        1,
        Config {
            size,
            throttle: Some(4 * MIB),
            ..Config::default()
        },
    );
    let result = result.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists());
}

#[test]
fn a_segmented_download_completes_when_only_the_final_record_cannot_be_written() {
    let size = 12 * MIB;
    let (_directory, output, result) = fetch_with_a_record_that_cannot_be_written(
        2,
        Config {
            size,
            throttle: Some(4 * MIB),
            ..Config::default()
        },
    );
    let result = result.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert!(!part_path(&output).exists());
    // The only cycle is the one at the end: it flushed through its own handle and both writers,
    // tried the record and could not write it.
    let mut tried = cycle_of(2, false);
    tried.push("failed");
    assert_eq!(cycles(), [tried], "{:?}", steps());
}

#[test]
fn a_segmented_download_stopped_after_its_final_record_failed_resumes_from_the_previous_record() {
    let size = 12 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(4 * MIB),
        ..Config::default()
    });
    // One periodic cycle records, and the record is held from then on: the cycle at the end
    // cannot write it. The download is stopped by the report that comes right before the file is
    // published.
    checkpoint_every(NEVER, 4 * MIB);
    let directory = TempDir::new("stopped-after-final-record");
    let output = directory.0.join("payload.bin");
    let state_file = state_path(&output);
    let control = TransferControl::default();
    let cancel = control.clone();
    let mut held = None;
    let mut observed = None;
    let completing = crate::i18n::ui("Tamamlanıyor", "Completing");
    let first = fetch(
        &request_for(&server),
        &output,
        &control,
        2,
        2,
        &mut |progress| {
            if held.is_none() && checkpoint_cycles() > 0 {
                held = Some(hold_against_replacement(&state_file));
                // The record that stays, read with the part file as it is while it is held.
                observed = read_state(&state_file).zip(fs::read(part_path(&output)).ok());
            }
            if progress.phase == completing {
                cancel.cancel();
            }
        },
    );
    assert!(held.is_some(), "the record was never held");
    drop(held);
    first.expect_err("the download was stopped before it was published");

    // The record is the one of the periodic cycle, as it was when it was held: every byte it
    // claims was in the part file then, and it does not claim them all.
    let (observed, part_then) = observed.expect("the record was never read");
    let claimed = assert_claims_are_exact(&observed, &part_then, &pattern(1, size));
    assert!(claimed > 0 && claimed < size, "{claimed} of {size} bytes");
    let state = read_state(&state_file).expect("the previous record stays");
    assert_eq!(state.completed, observed.completed);

    server.wait_idle();
    let first_attempt = server.log().len();
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
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    server.wait_idle();
    let mut requested = delivered(&server.log()[first_attempt..], size);
    requested.extend(state.completed.iter().map(|range| (range.start, range.end)));
    assert_tiles(requested, size);
}

#[test]
fn a_transport_error_stays_a_transport_error_when_the_record_cannot_be_written() {
    let size = 6 * MIB;
    let cut = 3 * MIB;
    let (_directory, output, result) = fetch_with_a_record_that_cannot_be_written(
        1,
        Config {
            size,
            throttle: Some(4 * MIB),
            cut_body: Some(CutBody {
                after_bytes: cut,
                responses: 1,
            }),
            ..Config::default()
        },
    );
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
        ..Config::default()
    });
    checkpoint_every(Duration::from_millis(100), u64::MAX);
    let directory = TempDir::new("cycle-order");
    let output = directory.0.join("payload.bin");
    // The download is held at `HELD_RATE` until two periodic cycles have recorded; the rest then
    // arrives, and a cycle at the end covers it.
    let control = TransferControl::default();
    control.set_speed_limit(HELD_RATE);
    let result = fetch(
        &request_for(&server),
        &output,
        &control,
        connections,
        connections,
        &mut |_| {
            if checkpoint_cycles() >= 2 {
                control.set_speed_limit(0);
            }
        },
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
    // The download is cut short once 6 MiB are written, which at this rate takes 24 seconds, so
    // the cycle that the clock starts has that long to be recorded. The server makes up for a
    // stall with a burst, so a faster rate would spend that time on the stall.
    let server = Server::start(Config {
        size,
        throttle: Some(128 * 1024),
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
                let at_cut = steps();
                seen = Some((
                    at_cut.iter().filter(|step| **step == "recorded").count(),
                    at_cut,
                    read_state(&state_file).zip(fs::read(part_path(&output)).ok()),
                ));
                cancel.cancel();
            }
        },
    );
    assert!(result.is_err());
    let (cycles, at_cut, record) = seen.expect("the download was never observed");
    assert!(
        cycles > 0,
        "no cycle was recorded before a lease could finish ({} steps, the first ones): {:?}",
        at_cut.len(),
        &at_cut[..at_cut.len().min(16)]
    );
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
fn a_cut_lease_that_has_everything_up_to_its_cut_closes_its_connection_although_the_server_is_silent(
) {
    let size = 12 * MIB + 4_097;
    // After the cut the stalled lease receives the bytes up to it, and the server sends nothing
    // more on that connection.
    let server = Server::start(Config {
        size,
        throttle: Some(24 * MIB),
        stall: Some((0, MIB)),
        hold_at_takeover: true,
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("silent-after-cut");
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
    assert_file_is(&result.path, &pattern(1, size), "silent after the cut");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    assert_eq!(steals(&log, size).len(), 1, "{log:?}");
    assert_eq!(server.abandoned_holds(), 1, "{log:?}");
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

/// Resumes a download of 16 MiB over 2 connections. The record leaves the first 8 MiB and the
/// last 64 KiB. The 64 KiB come over a connection at 0.25 MiB/s, of which the first `burst`
/// bytes arrive at once, while the lease of the first 8 MiB runs at 8 MiB/s. That connection
/// fetches no range of 2 MiB, so it takes no part of the lease however quickly its 64 KiB
/// arrive: the back half of the lease would take it at least 12 s, and the lease needs 1 s for
/// all. Asserts that the requests are the two for the gaps.
fn assert_a_slow_tail_takes_nothing_from_a_fast_lease(burst: u64) {
    let size = 16 * MIB;
    let tail = size - 64 * 1024;
    let source = pattern(1, size);
    let server = Server::start(Config {
        size,
        throttle: Some(8 * MIB),
        slow_connection: Some((0, MIB / 4)),
        burst,
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
fn a_slow_connection_with_nothing_left_does_not_take_the_back_of_a_fast_lease() {
    assert_a_slow_tail_takes_nothing_from_a_fast_lease(0);
}

#[test]
fn a_short_range_that_arrives_in_one_burst_does_not_make_its_connection_the_fastest() {
    // The 64 KiB are in before the first 256 KiB have gone by, so over the few milliseconds they
    // took the connection seems to fetch far faster than the lease, and a rate measured over
    // them would let it cut the lease and then fetch the back half at 0.25 MiB/s.
    assert_a_slow_tail_takes_nothing_from_a_fast_lease(256 * 1024);
}

#[test]
fn a_late_first_byte_keeps_an_idle_worker_from_taking_the_back_of_a_range_that_flows() {
    assert_a_late_first_byte_keeps_an_idle_worker_from_taking_the_back_of_a_range_that_flows();
}

#[test]
fn a_worker_that_starts_late_does_not_make_its_range_look_slow() {
    // The wait for the first byte counts from the moment the scheduler handed the range over, so
    // that the time a busy machine takes to start the worker is not taken for time in which the
    // range was flowing. Otherwise a worker that starts 200 ms late makes its range look slow
    // until it has flowed for a while, and an idle worker is given its back half.
    delay_range_starts_by(Duration::from_millis(200));
    assert_a_late_first_byte_keeps_an_idle_worker_from_taking_the_back_of_a_range_that_flows();
}

fn assert_a_late_first_byte_keeps_an_idle_worker_from_taking_the_back_of_a_range_that_flows() {
    // Three leases of 8 MiB over 2 connections at 8 MiB/s, and every ranged response, the probe
    // among them, starts a second after its request. The worker that finishes first takes the
    // last lease and the other has nothing to take. Once the last lease flows, a request for its
    // back half would wait a second for its first byte, longer than the half a second that the
    // half of at most 8 MiB would save the lease.
    let size = 24 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(8 * MIB),
        first_byte_delay: Some(Duration::from_secs(1)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("late-first-byte");
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
    assert_file_is(&result.path, &pattern(1, size), "a late first byte");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    let mut ranges = ranged_gets(&log, size);
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        [(0, 8 * MIB), (8 * MIB, 16 * MIB), (16 * MIB, size)],
        "{log:?}"
    );
}

#[test]
fn a_short_wait_for_the_first_byte_still_lets_an_idle_worker_take_the_back_of_a_range() {
    assert_a_short_wait_for_the_first_byte_lets_an_idle_worker_take_the_back_of_a_range();
}

#[test]
fn the_time_before_a_request_is_sent_is_not_a_wait_for_its_first_byte() {
    // The workers send their requests 1.2 s after the ranges are handed to them, as if each had
    // to renew an expired address first. The wait that an idle worker is expected to have for its
    // own request is the 0.1 s that the request took, not the 1.3 s since the hand-over, which
    // would keep it from taking the back half it is quicker with.
    delay_range_starts_by(Duration::from_millis(1200));
    assert_a_short_wait_for_the_first_byte_lets_an_idle_worker_take_the_back_of_a_range();
}

fn assert_a_short_wait_for_the_first_byte_lets_an_idle_worker_take_the_back_of_a_range() {
    // Three leases of 8 MiB over 2 connections at 4 MiB/s, every ranged response a tenth of a
    // second late. The worker that finishes first takes the last lease, the other is idle. The
    // last lease needs 2 s for its 8 MiB; the back half takes the idle worker 1 s at the rate of
    // its last range, after the 0.1 s it waits for the first byte.
    let size = 24 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(4 * MIB),
        first_byte_delay: Some(Duration::from_millis(100)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("short-first-byte");
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
    assert_file_is(&result.path, &pattern(1, size), "a short first byte wait");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    assert_tiles(delivered(&log, size), size);
    let found = steals(&log, size);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].victim, (16 * MIB, size));
    assert_eq!(ranged_gets(&log, size).len(), 4, "{log:?}");
}

#[test]
fn a_worker_that_has_closed_its_socket_waits_for_a_new_connection_before_it_takes_a_range() {
    // Five leases of 8 MiB over 2 connections at 8 MiB/s. Every ranged response starts 0.3 s
    // after its request, and the first request on a connection another 0.6 s later. The two
    // workers open their connections for the first two leases and reuse them for the next two.
    // The worker that finishes first takes the last lease, and the other is idle for more than
    // 0.2 s before that lease flows, so it has closed its socket. The response for the last
    // lease starts 1.2 s after its request, so that the other worker is idle that long
    // however far it runs behind the first. Its request for the back half would open a
    // connection and wait 0.9 s for the first byte, longer than the 0.45 s that the half of at
    // most 7.2 MiB would save the lease; on the connection it had left open it would have
    // waited 0.3 s.
    let size = 40 * MIB;
    let server = Server::start(Config {
        size,
        throttle: Some(8 * MIB),
        first_byte_delay: Some(Duration::from_millis(300)),
        late_first_byte_from: Some((4 * 8 * MIB, Duration::from_millis(1200))),
        connection_delay: Some(Duration::from_millis(600)),
        ..Config::default()
    });
    forget_writes();
    let directory = TempDir::new("closed-socket");
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
    assert_file_is(&result.path, &pattern(1, size), "a closed socket");
    assert_written_once(&writes(), &[], size);

    server.wait_idle();
    let log = server.log();
    let mut ranges = ranged_gets(&log, size);
    ranges.sort_unstable();
    assert_eq!(
        ranges,
        [
            (0, 8 * MIB),
            (8 * MIB, 16 * MIB),
            (16 * MIB, 24 * MIB),
            (24 * MIB, 32 * MIB),
            (32 * MIB, size)
        ],
        "{log:?}"
    );
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

/// A file request for `url`.
fn file_request(url: String) -> AddRequest {
    AddRequest {
        url,
        kind: crate::model::DownloadKind::File,
        ..Default::default()
    }
}

/// A server that sends the web page `kind` instead of a file: what a sign-in redirect ends on.
fn page_server(size: u64, kind: &str) -> Server {
    Server::start(Config {
        size,
        content_type: Some(kind.into()),
        ..Config::default()
    })
}

/// A server whose `GET` is redirected to `target`, as a sign-in redirect would be; its `HEAD` is
/// answered itself, as a file's metadata would.
fn redirecting_get(target: String, size: u64) -> Server {
    Server::start(Config {
        size,
        content_type: Some("application/octet-stream".into()),
        redirect_to: Some(target),
        head_not_redirected: true,
        ..Config::default()
    })
}

/// A server whose every request is redirected to `target`.
fn redirecting(target: String) -> Server {
    Server::start(Config {
        redirect_to: Some(target),
        ..Config::default()
    })
}

/// Asserts that a download was refused as a page that stands in for the file: the error is
/// SSD-NET-004, no file was published, and the part file holds no byte of the page.
fn assert_page_refused(error: &anyhow::Error, output: &Path) {
    assert_eq!(
        crate::error_codes::code_of(error),
        Some(crate::error_codes::NET_004),
        "{error:#}"
    );
    assert_eq!(
        crate::error_codes::from_text(&format!("{error:#}")),
        Some(crate::error_codes::NET_004),
        "the recorded text leads with the code"
    );
    assert!(!output.exists(), "the page was published");
    let part = part_path(output);
    assert!(
        fs::read(&part).map_or(true, |bytes| bytes.iter().all(|&byte| byte == 0)),
        "the page was written into the part file"
    );
    let kept = [part, state_path(output)].map(|path| path.file_name().unwrap().to_owned());
    for entry in fs::read_dir(output.parent().unwrap()).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(kept.contains(&name), "{name:?} was published");
    }
}

/// A file job whose request is redirected to a 4000-byte page, which answers HEAD with
/// `head_length` (its real length when `None`), fails before the page is fetched.
fn redirected_page_fails_before_a_byte_is_fetched(head_length: Option<u64>) {
    let page = Server::start(Config {
        size: 4_000,
        head_content_length: head_length,
        content_type: Some("text/html".into()),
        ..Config::default()
    });
    let source = redirecting(page.url("/signin"));
    let directory = TempDir::new("redirected-page");
    let output = directory.0.join("report.pdf");
    let error = fetch(
        &file_request(source.url("/file")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert_page_refused(&error, &output);
    page.wait_idle();
    assert!(
        page.log().iter().all(|entry| entry.method == "HEAD"),
        "{:?}",
        page.log()
    );
}

#[test]
fn a_file_job_redirected_to_a_page_fails_when_head_reports_a_length_of_zero() {
    redirected_page_fails_before_a_byte_is_fetched(Some(0));
}

#[test]
fn a_file_job_redirected_to_a_page_fails_when_head_reports_another_length_than_the_page_has() {
    redirected_page_fails_before_a_byte_is_fetched(Some(77));
}

#[test]
fn a_file_job_redirected_to_a_page_fails_when_head_reports_the_length_of_the_page() {
    redirected_page_fails_before_a_byte_is_fetched(None);
}

#[test]
fn a_redirect_to_a_page_is_refused_whatever_its_content_type_is_spelled_like() {
    for kind in [
        "application/xhtml+xml",
        "Text/HTML; charset=utf-8",
        "TEXT/html;charset=ISO-8859-1",
    ] {
        let page = page_server(4_000, kind);
        let source = redirecting(page.url("/signin"));
        let directory = TempDir::new("redirected-page-type");
        let output = directory.0.join("report.pdf");
        let error = fetch(
            &file_request(source.url("/file")),
            &output,
            &TransferControl::default(),
            1,
            1,
            &mut |_| {},
        )
        .unwrap_err();
        assert_page_refused(&error, &output);
    }
}

#[test]
fn a_page_reached_after_a_redirect_in_the_transfer_is_not_written_by_a_one_connection_download() {
    for kind in [
        crate::model::DownloadKind::File,
        crate::model::DownloadKind::Auto,
    ] {
        // The probe's HEAD sees the file; the GET is redirected to the page.
        let page = page_server(4_000, "text/html");
        let source = redirecting_get(page.url("/signin"), 4_000);
        let directory = TempDir::new("redirected-get");
        let output = directory.0.join("report.pdf");
        let mut request = file_request(source.url("/file"));
        request.kind = kind;
        let control = TransferControl::default();
        let network = NetworkGovernor::new(1);
        let (media, prepared) = if kind == crate::model::DownloadKind::Auto {
            prepare_auto(&request, &control, &network).unwrap()
        } else {
            (false, None)
        };
        assert!(!media, "{kind:?}");
        let error = download(
            &request,
            &output,
            &control,
            &network,
            1,
            prepared,
            &mut |_| {},
        )
        .unwrap_err();
        assert_page_refused(&error, &output);
        page.wait_idle();
        assert!(
            page.log().iter().any(|entry| entry.method == "GET"),
            "{kind:?}: the transfer should have reached the page"
        );
    }
}

/// A source that is redirected as a whole to a 6 MiB resource which is a file for the first
/// `file_requests` requests and a page afterwards.
fn segmented_page(file_requests: usize) -> (Server, Server) {
    let target = Server::start(Config {
        size: 6 * MIB,
        content_type: Some("application/octet-stream".into()),
        content_type_from: Some((file_requests, "text/html".into())),
        ..Config::default()
    });
    let source = redirecting(target.url("/signin"));
    (source, target)
}

#[test]
fn a_page_in_the_range_support_probe_is_not_downloaded_over_several_connections() {
    // The HEAD (request 0) sees the file; the one-byte probe (request 1) sees the page.
    let (source, target) = segmented_page(1);
    let directory = TempDir::new("redirected-probe-page");
    let output = directory.0.join("archive.zip");
    forget_writes();
    let error = fetch(
        &file_request(source.url("/file")),
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |_| {},
    )
    .unwrap_err();
    assert_page_refused(&error, &output);
    assert!(writes().is_empty());
    target.wait_idle();
    assert_eq!(data_gets(&target.log()).count(), 0, "{:?}", target.log());
}

#[test]
fn a_page_in_a_ranged_transfer_is_not_written_by_any_connection() {
    // The HEAD and the one-byte probe (requests 0 and 1) see the file; every range sees the page.
    let (source, target) = segmented_page(2);
    let directory = TempDir::new("redirected-range-page");
    let output = directory.0.join("archive.zip");
    forget_writes();
    let error = fetch(
        &file_request(source.url("/file")),
        &output,
        &TransferControl::default(),
        4,
        4,
        &mut |_| {},
    )
    .unwrap_err();
    assert_page_refused(&error, &output);
    assert!(writes().is_empty(), "{:?}", writes());
    target.wait_idle();
    assert!(
        data_gets(&target.log()).count() > 0,
        "the ranges should have reached the page: {:?}",
        target.log()
    );
}

/// Downloads from a source that redirects to `target` into a file called `name`. The directory
/// is returned so that the file outlives the call.
fn fetch_redirected(target: &Server, name: &str) -> (Result<TransferResult>, TempDir) {
    let source = redirecting(target.url("/landing"));
    let directory = TempDir::new("redirected-allowed");
    let output = directory.0.join(name);
    let result = fetch(
        &file_request(source.url("/file")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    );
    (result, directory)
}

#[test]
fn a_redirect_to_a_file_or_to_a_download_that_names_itself_keeps_downloading() {
    let size = 4_000;
    // A file after a redirect.
    let file = page_server(size, "application/octet-stream");
    let (result, _directory) = fetch_redirected(&file, "report.pdf");
    assert_eq!(fs::read(result.unwrap().path).unwrap(), pattern(1, size));

    // A page that the server sends as an attachment, in either spelling of the header.
    for disposition in ["attachment; filename=\"saved.bin\"", "Attachment"] {
        let page = Server::start(Config {
            size,
            content_type: Some("text/html".into()),
            content_disposition: Some(disposition.into()),
            ..Config::default()
        });
        let (result, _directory) = fetch_redirected(&page, "report.pdf");
        assert_eq!(
            fs::read(result.unwrap().path).unwrap(),
            pattern(1, size),
            "{disposition}"
        );
    }

    // A page that the user asked to save under the name of a page.
    for name in ["page.html", "page.HTM", "page.xhtml", "page (1).html"] {
        let page = page_server(size, "text/html");
        let (result, _directory) = fetch_redirected(&page, name);
        assert_eq!(
            fs::read(result.unwrap().path).unwrap(),
            pattern(1, size),
            "{name}"
        );
    }
}

#[test]
fn a_page_that_is_reached_without_a_redirect_is_still_saved() {
    let size = 4_000;
    let page = page_server(size, "text/html");
    let directory = TempDir::new("direct-page");
    let output = directory.0.join("page");
    let result = fetch(
        &file_request(page.url("/page")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(fs::read(result.path).unwrap(), pattern(1, size));
}

#[test]
fn an_auto_job_redirected_to_a_page_stays_a_media_job() {
    let page = page_server(4_000, "text/html");
    let source = redirecting(page.url("/watch"));
    let request = AddRequest {
        url: source.url("/short"),
        ..Default::default()
    };
    let (media, prepared) = prepare_auto(
        &request,
        &TransferControl::default(),
        &NetworkGovernor::new(1),
    )
    .unwrap();
    assert!(media);
    assert!(prepared.is_some());
}

#[test]
fn a_redirect_inside_one_origin_to_a_page_is_refused() {
    let site = Server::start(Config {
        size: 4_000,
        content_type: Some("text/html".into()),
        redirect: Some(Redirect {
            from: "/download".into(),
            target: "/login".into(),
            fresh_target: "/login".into(),
            target_requests_before_403: usize::MAX,
        }),
        ..Config::default()
    });
    let directory = TempDir::new("same-origin-page");
    let output = directory.0.join("report.pdf");
    let error = fetch(
        &file_request(site.url("/download")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert_page_refused(&error, &output);
    site.wait_idle();
    let log = site.log();
    let reached = log
        .iter()
        .map(|entry| (entry.method.as_str(), entry.path.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        reached,
        [("HEAD", "/download"), ("HEAD", "/login")],
        "{log:?}"
    );
}

#[test]
fn a_redirect_taken_by_the_transfer_to_a_page_is_saved_under_the_name_of_a_page() {
    let size = 4_000;
    for name in ["page.html", "page.HTM", "page.xhtml"] {
        // The probe's HEAD sees the file, so the redirect is first met by the transfer's GET.
        let page = page_server(size, "text/html");
        let source = redirecting_get(page.url("/landing"), size);
        let directory = TempDir::new("redirected-get-page-name");
        let output = directory.0.join(name);
        let result = fetch(
            &file_request(source.url("/file")),
            &output,
            &TransferControl::default(),
            1,
            1,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{name}: {error:#}"));
        assert_eq!(fs::read(result.path).unwrap(), pattern(1, size), "{name}");
        page.wait_idle();
        assert!(
            page.log().iter().any(|entry| entry.method == "GET"),
            "{name}: the transfer should have reached the page"
        );
    }
}

/// What a resume record claims, apart from the address digest and the synced length that an
/// attempt that resumes rewrites.
type Claims = (
    Option<String>,
    Option<String>,
    Option<u64>,
    bool,
    Vec<(u64, u64)>,
);

fn claims_of(output: &Path) -> Option<Claims> {
    read_state(&state_path(output)).map(|state| {
        (
            state.etag,
            state.last_modified,
            state.total,
            state.segmented,
            state
                .completed
                .iter()
                .map(|range| (range.start, range.end))
                .collect(),
        )
    })
}

/// Resumes the download that an earlier attempt left in `output`, whose request now reaches a
/// page, and asserts that the page is refused and that the part file and its record are as
/// they were: the retained work was neither discarded nor restarted.
fn assert_a_page_leaves_the_retained_work_alone(url: String, output: &Path, connections: u8) {
    let part = fs::read(part_path(output)).unwrap();
    let claims = claims_of(output);
    let retained = retained_bytes(output);
    assert!(claims.is_some() && retained.is_some());

    let control = TransferControl::default();
    let error = fetch(
        &file_request(url),
        output,
        &control,
        connections,
        connections,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(
        crate::error_codes::code_of(&error),
        Some(crate::error_codes::NET_004),
        "{error:#}"
    );
    assert!(!output.exists(), "the page was published");
    assert!(!control.restarted(), "the retained work was discarded");
    assert!(
        fs::read(part_path(output)).unwrap() == part,
        "the part file changed"
    );
    assert_eq!(claims_of(output), claims);
    assert_eq!(retained_bytes(output), retained);
}

/// A source that is redirected as a whole to a 6 MiB resource which is a file for its first
/// `file_requests` requests and a page afterwards, with validators of its own.
fn segmented_page_with_other_validators(file_requests: usize) -> (Server, Server) {
    let target = Server::start(Config {
        size: 6 * MIB,
        content_type: Some("application/octet-stream".into()),
        content_type_from: Some((file_requests, "text/html".into())),
        switch_version_after: Some(file_requests),
        ..Config::default()
    });
    let source = redirecting(target.url("/signin"));
    (source, target)
}

#[test]
fn a_resumed_one_connection_download_that_is_redirected_to_a_page_keeps_its_part_file() {
    let size = MIB;
    let held = 300_001usize;
    let pages = [
        // A sign-in page: no validators, and a ranged request is answered with the whole page.
        Config {
            size: 4_000,
            content_type: Some("text/html".into()),
            etag: EtagKind::Absent,
            last_modified: false,
            ranges: RangeSupport::Ignore,
            ..Config::default()
        },
        // A page with validators of its own.
        Config {
            size: 4_000,
            content_type: Some("text/html".into()),
            switch_version_after: Some(0),
            ..Config::default()
        },
    ];
    for (index, page) in pages.into_iter().enumerate() {
        // The probe's HEAD sees the file, so the resume goes ahead and its GET meets the page.
        let page = Server::start(page);
        let source = redirecting_get(page.url("/signin"), size);
        let directory = TempDir::new("resume-into-page");
        let output = directory.0.join("payload.bin");
        fs::write(part_path(&output), &pattern(1, size)[..held]).unwrap();
        fs::write(
            state_path(&output),
            previous_release_state(&source, size, false, "[]"),
        )
        .unwrap();

        assert_a_page_leaves_the_retained_work_alone(source.url("/payload.bin"), &output, 1);
        page.wait_idle();
        let log = page.log();
        let gets = log
            .iter()
            .filter(|entry| entry.method == "GET")
            .collect::<Vec<_>>();
        assert_eq!(gets.len(), 1, "page {index}: {log:?}");
        assert_eq!(
            gets[0].range,
            Some(format!("bytes={held}-")),
            "page {index}"
        );
        assert_eq!(gets[0].if_range.as_deref(), Some("\"v1\""), "page {index}");
    }
}

#[test]
fn a_resumed_segmented_download_whose_ranges_meet_a_page_keeps_its_part_file() {
    let size = 6 * MIB;
    let done = MIB;
    // The HEAD (request 0) sees the file; every range request after it meets the page.
    let (source, target) = segmented_page_with_other_validators(1);
    let directory = TempDir::new("resume-ranges-into-page");
    let output = directory.0.join("payload.bin");
    let mut part = File::create(part_path(&output)).unwrap();
    part.write_all(&pattern(1, size)[..done as usize]).unwrap();
    part.set_len(size).unwrap();
    drop(part);
    fs::write(
        state_path(&output),
        previous_release_state(
            &source,
            size,
            true,
            &format!(r#"[{{"start":0,"end":{done}}}]"#),
        ),
    )
    .unwrap();

    assert_a_page_leaves_the_retained_work_alone(source.url("/payload.bin"), &output, 2);
    target.wait_idle();
    let log = target.log();
    assert!(
        data_gets(&log).any(|entry| entry.range.is_some()),
        "the ranges should have reached the page: {log:?}"
    );
}

#[test]
fn a_resumed_download_whose_range_probe_meets_a_page_keeps_its_part_file() {
    let size = 6 * MIB;
    // The HEAD (request 0) sees the file; the one-byte probe after it meets the page. The
    // record holds no range yet, so the probe is what decides whether the ranges can be used.
    let (source, target) = segmented_page_with_other_validators(1);
    let directory = TempDir::new("resume-probe-into-page");
    let output = directory.0.join("payload.bin");
    File::create(part_path(&output))
        .unwrap()
        .set_len(size)
        .unwrap();
    fs::write(
        state_path(&output),
        previous_release_state(&source, size, true, "[]"),
    )
    .unwrap();

    assert_a_page_leaves_the_retained_work_alone(source.url("/payload.bin"), &output, 2);
    target.wait_idle();
    let log = target.log();
    let gets = log
        .iter()
        .filter(|entry| entry.method == "GET")
        .collect::<Vec<_>>();
    assert_eq!(gets.len(), 1, "{log:?}");
    assert_eq!(gets[0].range.as_deref(), Some("bytes=0-0"), "{log:?}");
    assert_eq!(gets[0].if_range.as_deref(), Some("\"v1\""), "{log:?}");
}

/// Signed addresses that live `milliseconds`, however often they are asked for.
fn lasting(milliseconds: u64) -> Validity {
    Validity {
        lifetime: Duration::from_millis(milliseconds),
        requests: usize::MAX,
    }
}

/// A source whose `/file` redirects to a new signed address on every request, and which answers
/// `status` for an address that has expired. It serves `size` bytes at `rate` per connection.
fn signed_source(size: u64, rate: u64, status: u16, validity: Vec<Validity>) -> Server {
    Server::start(Config {
        size,
        throttle: Some(rate),
        signing: Some(Signing {
            from: "/file".into(),
            origin: String::new(),
            validity,
        }),
        expiry: Some(status),
        ..Config::default()
    })
}

/// The number of the signed address that `entry` asked for.
fn address_of(entry: &Logged) -> Option<u64> {
    number_of(&entry.path)
}

fn number_of(path: &str) -> Option<u64> {
    path.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix("n=")?.parse().ok())
}

fn requests_for_the_source(log: &[Logged]) -> usize {
    log.iter().filter(|entry| entry.path == "/file").count()
}

/// What a signed address was asked for, as (method, range, status).
fn asked_of(log: &[Logged], address: u64) -> Vec<(&str, Option<&str>, u16)> {
    log.iter()
        .filter(|entry| address_of(entry) == Some(address))
        .map(|entry| (entry.method.as_str(), entry.range.as_deref(), entry.status))
        .collect()
}

/// Asserts that the ranges of `log` that were answered with data tile `0..size`.
fn assert_served_tiles(log: &[Logged], size: u64) {
    let served = log
        .iter()
        .filter(|entry| entry.status == 206)
        .cloned()
        .collect::<Vec<_>>();
    assert_tiles(delivered(&served, size), size);
}

const SIGNED_SIZE: u64 = 20 * MIB + 123;
const SIGNED_RATE: u64 = 8 * MIB;

#[test]
fn a_segmented_download_follows_the_source_again_when_its_redirect_target_expires() {
    let expected = pattern(1, SIGNED_SIZE);
    for status in [401, 403, 410] {
        // The first address serves the probe's two requests and the first two ranges, so the
        // third range meets the expiry.
        let server = signed_source(SIGNED_SIZE, SIGNED_RATE, status, expiring_after(4));
        let directory = TempDir::new("signed-segmented");
        let output = directory.0.join("payload.bin");
        forget_writes();
        let result = fetch(
            &file_request(server.url("/file")),
            &output,
            &TransferControl::default(),
            2,
            2,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{status}: {error:#}"));
        assert_file_is(&result.path, &expected, &status.to_string());
        assert_written_once(&writes(), &[], SIGNED_SIZE);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        server.wait_idle();
        let log = server.log();
        // The probe asks for the source, and the expiry makes the download ask once more.
        assert_eq!(requests_for_the_source(&log), 2, "{status}: {log:?}");
        let refused = log.iter().filter(|entry| entry.status == status);
        assert!(refused.clone().count() > 0, "{status}: nothing expired");
        assert!(
            refused.clone().all(|entry| address_of(entry) == Some(0)),
            "{status}: {log:?}"
        );
        assert!(
            data_gets(&log).any(|entry| entry.status == 206 && address_of(entry) == Some(1)),
            "{status}: the second address served no data: {log:?}"
        );
        // Every request that continues the bytes on disk carries the validator, the one that
        // checks the second address included.
        for entry in log
            .iter()
            .filter(|entry| entry.method == "GET" && address_of(entry) == Some(1))
        {
            assert_eq!(
                entry.if_range.as_deref(),
                Some("\"v1\""),
                "{status}: {entry:?}"
            );
        }
        assert_served_tiles(&log, SIGNED_SIZE);
    }
}

#[test]
fn a_segmented_download_follows_the_source_again_at_each_expiry_that_follows_progress() {
    let size = 40 * MIB + 7;
    // Each of the first two addresses serves the two requests that check it and two ranges, and
    // the third never expires.
    let server = signed_source(
        size,
        SIGNED_RATE,
        403,
        vec![valid_for(4), valid_for(4), valid_for(usize::MAX)],
    );
    let directory = TempDir::new("signed-repeated");
    let output = directory.0.join("payload.bin");
    forget_writes();
    let result = fetch(
        &file_request(server.url("/file")),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap_or_else(|error| panic!("{error:#}"));
    assert_file_is(&result.path, &pattern(1, size), "repeated");
    assert_written_once(&writes(), &[], size);
    server.wait_idle();
    let log = server.log();
    // Two leases are requested at the first expiry and one at the second: the second address
    // delivered bytes before it expired, which allows a third.
    assert_eq!(requests_for_the_source(&log), 3, "{log:?}");
    assert_served_tiles(&log, size);
}

#[test]
fn a_one_connection_download_follows_the_source_again_when_a_retry_meets_an_expired_target() {
    let size = 4 * MIB;
    for status in [401, 403, 410] {
        let server = Server::start(Config {
            size,
            signing: Some(Signing {
                from: "/file".into(),
                origin: String::new(),
                validity: expiring_after(2),
            }),
            expiry: Some(status),
            // The first body is cut before its first byte, so the download waits a second and
            // asks the same address again, which has served its probe and that body.
            cut_body: Some(CutBody {
                after_bytes: 0,
                responses: 1,
            }),
            ..Config::default()
        });
        let directory = TempDir::new("signed-single");
        let output = directory.0.join("payload.bin");
        let result = fetch(
            &file_request(server.url("/file")),
            &output,
            &TransferControl::default(),
            1,
            1,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{status}: {error:#}"));
        assert_file_is(&result.path, &pattern(1, size), &status.to_string());
        server.wait_idle();
        let log = server.log();
        assert_eq!(requests_for_the_source(&log), 2, "{status}: {log:?}");
        assert!(
            log.iter()
                .filter(|entry| entry.status == status)
                .all(|entry| address_of(entry) == Some(0)),
            "{status}: {log:?}"
        );
        assert_eq!(
            data_gets(&log)
                .filter(|entry| entry.status == 200)
                .map(address_of)
                .collect::<Vec<_>>(),
            [Some(0), Some(1)],
            "{status}: {log:?}"
        );
    }
}

#[test]
fn a_download_resumed_after_its_redirect_target_expired_asks_the_source_again() {
    // The first attempt is stopped once its leases have written bytes, and the address it used
    // has expired when the second one starts: it served the requests of the first attempt (the
    // probe's HEAD, the ranged probe of a segmented download, and one range per connection).
    let size = 12 * MIB + 123;
    let expected = pattern(1, size);
    for (connections, first_attempt_requests) in [(1u8, 2), (2, 4)] {
        let server = signed_source(
            size,
            SIGNED_RATE,
            403,
            expiring_after(first_attempt_requests),
        );
        let directory = TempDir::new("signed-resume");
        let output = directory.0.join("payload.bin");
        let control = TransferControl::default();
        let first = fetch(
            &file_request(server.url("/file")),
            &output,
            &control,
            connections,
            connections,
            &mut |progress| {
                if progress.downloaded > 0 {
                    control.cancel();
                }
            },
        );
        assert!(first.is_err(), "{connections}: the first attempt finished");
        assert!(
            retained_bytes(&output).is_some_and(|kept| kept > 0),
            "{connections}: nothing was kept"
        );
        let recorded = recorded_ranges(&output);
        server.wait_idle();
        let log = server.log();
        assert_eq!(
            asked_of(&log, 0).len(),
            first_attempt_requests,
            "{connections}: the first attempt did not use up the address: {log:?}"
        );
        let first_attempt = log.len();

        forget_writes();
        let result = fetch(
            &file_request(server.url("/file")),
            &output,
            &TransferControl::default(),
            connections,
            connections,
            &mut |_| {},
        )
        .unwrap_or_else(|error| panic!("{connections}: {error:#}"));
        assert_file_is(&result.path, &expected, &connections.to_string());
        if connections > 1 {
            assert_written_once(&writes(), &recorded, size);
        }
        server.wait_idle();
        let log = server.log();
        let resumed = &log[first_attempt..];
        assert!(
            resumed.iter().all(|entry| entry.status != 403),
            "{connections}: {resumed:?}"
        );
        for entry in data_gets(resumed) {
            assert_eq!(entry.status, 206, "{connections}: {entry:?}");
            assert_eq!(entry.if_range.as_deref(), Some("\"v1\""), "{entry:?}");
            assert_ne!(address_of(entry), Some(0), "{connections}: {entry:?}");
        }
    }
}

/// Downloads `SIGNED_SIZE` bytes over two connections from a signed source that hands out
/// `validity`, expects the download to fail with `status` and the work done before the expiry
/// to stay recorded, and returns the source.
fn signed_download_that_fails_with(
    status: u16,
    validity: Vec<Validity>,
    tweak: impl Fn(&mut Config),
) -> (Server, TempDir) {
    let mut config = Config {
        size: SIGNED_SIZE,
        throttle: Some(SIGNED_RATE),
        signing: Some(Signing {
            from: "/file".into(),
            origin: String::new(),
            validity,
        }),
        expiry: Some(status),
        ..Config::default()
    };
    tweak(&mut config);
    let server = Server::start(config);
    let directory = TempDir::new("signed-fails");
    let output = directory.0.join("payload.bin");
    let error = fetch(
        &file_request(server.url("/file")),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(http_status(&error), Some(u32::from(status)), "{error:#}");
    assert!(!output.exists());
    let expected = pattern(1, SIGNED_SIZE);
    let claimed = claimed_bytes(&output, &expected);
    assert!(claimed >= 8 * MIB, "only {claimed} bytes were kept");
    assert_eq!(retained_bytes(&output), Some(claimed));
    server.wait_idle();
    (server, directory)
}

/// Whether the requests for `address` are only the ones that check it: a HEAD and the one-byte
/// range, never data.
fn only_checked(log: &[Logged], address: u64) -> bool {
    asked_of(log, address)
        .iter()
        .all(|(method, range, _)| *method == "HEAD" || *range == Some("bytes=0-0"))
}

#[test]
fn a_fresh_target_with_another_identity_is_not_adopted_and_the_retained_work_stays() {
    // The tag changes with each HEAD, so the source's answer after the expiry names another
    // file than the one whose bytes are on disk.
    let (server, _directory) = signed_download_that_fails_with(403, expiring_after(4), |config| {
        config.etag_rotation_heads = Some(1);
    });
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert!(
        !asked_of(&log, 1).is_empty() && only_checked(&log, 1),
        "{log:?}"
    );
}

#[test]
fn a_fresh_target_that_answers_again_with_the_status_is_not_asked_for_data() {
    let (server, _directory) = signed_download_that_fails_with(
        410,
        vec![
            valid_for(4),
            Validity {
                lifetime: Duration::ZERO,
                requests: usize::MAX,
            },
        ],
        |_| {},
    );
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert!(
        asked_of(&log, 1)
            .iter()
            .all(|(_, _, status)| *status == 410),
        "{log:?}"
    );
    assert!(only_checked(&log, 1), "{log:?}");
}

#[test]
fn a_fresh_target_that_delivers_nothing_is_followed_only_once() {
    // The second address answers the two requests that check it and refuses everything after
    // them, so its data requests are refused without a byte having come from it.
    let (server, _directory) =
        signed_download_that_fails_with(403, vec![valid_for(4), valid_for(2)], |_| {});
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    let asked = asked_of(&log, 1);
    assert!(asked.len() >= 3, "{log:?}");
    assert!(
        asked[2..]
            .iter()
            .all(|(_, range, status)| *range != Some("bytes=0-0") && *status == 403),
        "{log:?}"
    );
}

#[test]
fn a_fresh_target_that_is_a_web_page_is_not_adopted_and_the_retained_work_stays() {
    let (server, _directory) = signed_download_that_fails_with(403, expiring_after(4), |config| {
        config.signed_pages_from = Some(1);
    });
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert!(
        !asked_of(&log, 1).is_empty() && only_checked(&log, 1),
        "{log:?}"
    );
}

#[test]
fn a_one_connection_download_follows_the_source_only_once_when_the_fresh_target_delivers_nothing() {
    // The second address answers the requests that check it and refuses the one for data.
    let size = 4 * MIB;
    let server = Server::start(Config {
        size,
        signing: Some(Signing {
            from: "/file".into(),
            origin: String::new(),
            validity: vec![valid_for(2), valid_for(2)],
        }),
        expiry: Some(403),
        cut_body: Some(CutBody {
            after_bytes: 0,
            responses: 1,
        }),
        ..Config::default()
    });
    let directory = TempDir::new("signed-single-once");
    let output = directory.0.join("payload.bin");
    let error = fetch(
        &file_request(server.url("/file")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(http_status(&error), Some(403), "{error:#}");
    server.wait_idle();
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert_eq!(
        asked_of(&log, 1)
            .iter()
            .map(|(method, _, status)| (*method, *status))
            .collect::<Vec<_>>(),
        [("HEAD", 200), ("GET", 206), ("GET", 403)],
        "{log:?}"
    );
}

#[test]
fn an_address_with_no_redirect_that_expires_is_not_followed_again() {
    let server = Server::start(Config {
        size: SIGNED_SIZE,
        throttle: Some(SIGNED_RATE),
        expiry: Some(403),
        ..Config::default()
    });
    let directory = TempDir::new("signed-direct");
    let output = directory.0.join("payload.bin");
    // The address answers the probe's two requests and the first two ranges, so the third range
    // meets the expiry however long the requests take.
    let error = fetch(
        &file_request(server.url("/direct?exp=99999999999999&n=0&max=4")),
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(http_status(&error), Some(403), "{error:#}");
    server.wait_idle();
    let log = server.log();
    // Only the probe asks for metadata; nothing asks the address again after the refusal.
    assert_eq!(
        log.iter().filter(|entry| entry.method == "HEAD").count(),
        1,
        "{log:?}"
    );
    assert!(data_gets(&log).any(|entry| entry.status == 403), "{log:?}");
}

#[test]
fn the_requests_that_follow_an_expiry_keep_the_headers_of_the_origin_they_go_to() {
    let watched = [
        "authorization",
        "cookie",
        "referer",
        "x-site-token",
        "accept",
    ];
    let target = Server::start(Config {
        size: SIGNED_SIZE,
        throttle: Some(SIGNED_RATE),
        expiry: Some(403),
        logged_headers: watched.map(String::from).to_vec(),
        ..Config::default()
    });
    let source = Server::start(Config {
        signing: Some(Signing {
            from: "/file".into(),
            origin: target.url(""),
            validity: expiring_after(4),
        }),
        logged_headers: watched.map(String::from).to_vec(),
        ..Config::default()
    });
    let mut request = file_request(source.url("/file"));
    request
        .headers
        .insert("Authorization".into(), "Bearer secret".into());
    request
        .headers
        .insert("Cookie".into(), "sid=private".into());
    request
        .headers
        .insert("X-Site-Token".into(), "do-not-forward".into());
    request.headers.insert("Accept".into(), "*/*".into());
    request.referer = Some(source.url("/page"));
    let directory = TempDir::new("signed-headers");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        2,
        2,
        &mut |_| {},
    )
    .unwrap_or_else(|error| panic!("{error:#}"));
    assert_file_is(&result.path, &pattern(1, SIGNED_SIZE), "headers");
    target.wait_idle();
    source.wait_idle();
    let target_log = target.log();
    assert!(
        target_log.iter().any(|entry| entry.status == 403),
        "the address never expired: {target_log:?}"
    );
    for entry in &target_log {
        for name in ["authorization", "cookie", "referer", "x-site-token"] {
            assert_eq!(entry.header(name), None, "{name}: {entry:?}");
        }
        assert_eq!(entry.header("accept"), Some("*/*"), "{entry:?}");
    }
    // The probe and the request after the expiry each ask the source with a HEAD.
    let source_log = source.log();
    assert_eq!(source_log.len(), 2, "{source_log:?}");
    for entry in &source_log {
        assert_eq!(entry.header("authorization"), Some("Bearer secret"));
        assert_eq!(entry.header("cookie"), Some("sid=private"));
        assert_eq!(entry.header("x-site-token"), Some("do-not-forward"));
    }
}

#[test]
fn a_fresh_target_is_not_accepted_when_it_is_less_secure_than_the_one_that_expired() {
    let server = signed_source(SIGNED_SIZE, SIGNED_RATE, 403, vec![lasting(60_000)]);
    let request = file_request(server.url("/file"));
    let control = TransferControl::default();
    let network = NetworkGovernor::new(2);
    let original = Url::parse(&request.url).unwrap();
    let record = probe_http(&request, original.clone(), &control, &network).unwrap();
    let before = server.log().len();

    let secure = Url::parse("https://files.example/signed").unwrap();
    assert!(refollow(&request, &original, &secure, &record, &control, &network).is_none());
    assert!(
        server.log()[before..]
            .iter()
            .all(|entry| entry.method == "HEAD"),
        "the insecure target was asked for data"
    );

    let plain = Url::parse("http://files.example/signed").unwrap();
    let fresh = refollow(&request, &original, &plain, &record, &control, &network).unwrap();
    assert_eq!(number_of(fresh.as_str()), Some(2));
}

/// A signed address that answers its first `requests` requests, however long they take to
/// arrive, and as expired from the next on.
fn valid_for(requests: usize) -> Validity {
    Validity {
        lifetime: Duration::from_secs(3600),
        requests,
    }
}

/// Signed addresses of which the first answers as expired from its request after the first
/// `requests` on. The addresses after the first never expire.
fn expiring_after(requests: usize) -> Vec<Validity> {
    vec![valid_for(requests), valid_for(usize::MAX)]
}

#[test]
fn a_fresh_target_whose_ranged_answer_carries_no_validators_is_not_adopted() {
    // The answers to ranged requests with If-Range have neither ETag nor Last-Modified, so the
    // fresh target proves nothing about the file whose bytes are on disk.
    let (server, _directory) = signed_download_that_fails_with(403, expiring_after(4), |config| {
        config.partial_without_validators = true;
    });
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert!(
        !asked_of(&log, 1).is_empty() && only_checked(&log, 1),
        "{log:?}"
    );
}

#[test]
fn a_fresh_target_whose_one_byte_answer_has_no_body_is_checked_all_the_same() {
    // The check sends no body byte, so nothing but the headers can prove the identity.
    let (server, _directory) = signed_download_that_fails_with(403, expiring_after(4), |config| {
        config.partial_without_validators = true;
        config.bodiless_probe = true;
    });
    let log = server.log();
    assert_eq!(requests_for_the_source(&log), 2, "{log:?}");
    assert!(
        !asked_of(&log, 1).is_empty() && only_checked(&log, 1),
        "{log:?}"
    );
}

#[test]
fn a_fresh_target_on_another_host_gets_a_new_handle_and_not_the_site_login_of_the_old_host() {
    let size = 40 * MIB + 123;
    let target = |size| Config {
        size,
        throttle: Some(SIGNED_RATE),
        expiry: Some(403),
        ..Config::default()
    };
    // The second lease of the old host outlasts the renewal, so its worker meets the fresh host
    // with the handle it has from the old one.
    let old = Server::start(Config {
        slow_connection: Some((0, SIGNED_RATE * 3 / 8)),
        ..target(size)
    });
    let fresh = Server::start(target(size));
    // The first address is on `localhost`, the ones after it on `127.0.0.1`: two hosts.
    let source = Server::start(Config {
        signing: Some(Signing {
            from: "/file".into(),
            origin: old.url("").replace("127.0.0.1", "localhost"),
            validity: expiring_after(4),
        }),
        signed_origin_from: Some((1, fresh.url(""))),
        ..Config::default()
    });
    // The logins are process-wide and other tests reset them, so they are installed again
    // until the download ends.
    let login = crate::model::SiteLogin {
        host: "localhost".into(),
        username: "user".into(),
        password: String::new(),
        allow_http: true,
    };
    let installed = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let directory = TempDir::new("signed-other-host");
    let output = directory.0.join("payload.bin");
    let result = thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Acquire) {
                crate::proxy::configure_site_logins(std::slice::from_ref(&login));
                installed.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(2));
            }
        });
        while !installed.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        let result = fetch(
            &file_request(source.url("/file")),
            &output,
            &TransferControl::default(),
            2,
            2,
            &mut |_| {},
        );
        finished.store(true, Ordering::Release);
        result
    });
    crate::proxy::configure_site_logins(&[]);
    let result = result.unwrap_or_else(|error| {
        panic!(
            "{error:#}: {:?} {:?} {:?}",
            old.log(),
            fresh.log(),
            source.log()
        )
    });
    assert_file_is(&result.path, &pattern(1, size), "other host");
    old.wait_idle();
    fresh.wait_idle();
    let (old_log, fresh_log) = (old.log(), fresh.log());
    assert!(
        old_log.iter().any(|entry| entry
            .header("authorization")
            .is_some_and(|value| value.starts_with("Basic "))),
        "the login never reached its own host: {old_log:?}"
    );
    assert!(
        fresh_log.iter().any(|entry| entry.status == 206),
        "the fresh host served no data: {fresh_log:?}"
    );
    for entry in &fresh_log {
        assert_eq!(entry.header("authorization"), None, "{entry:?}");
    }
}

#[test]
fn a_renewal_does_not_stall_the_download_while_another_job_takes_the_slot_it_gave_up() {
    // The original address is on a slow host of its own and the signed addresses are on the
    // host that another job also downloads from, under a limit of two connections per host. The
    // refused worker frees its slot and the other job takes it; the download must not wait for
    // that job to end.
    let size = 40 * MIB + 123;
    let target = Server::start(Config {
        size,
        throttle: Some(5 * MIB),
        // The second lease runs long after the first one ends and its worker holds a slot.
        slow_connection: Some((0, 2 * MIB)),
        expiry: Some(403),
        ..Config::default()
    });
    let source = Server::start(Config {
        connection_delay: Some(Duration::from_millis(500)),
        signing: Some(Signing {
            from: "/file".into(),
            origin: target.url(""),
            validity: expiring_after(4),
        }),
        ..Config::default()
    });
    let rival = Server::start(Config {
        size: 64 * MIB,
        throttle: Some(2 * MIB),
        ..Config::default()
    });
    let network = NetworkGovernor::new(2);
    let directory = TempDir::new("signed-rival");
    let output = directory.0.join("payload.bin");
    let rival_output = directory.0.join("rival.bin");
    let rival_control = TransferControl::default();
    let (done_tx, done_rx) = mpsc::channel();
    let (first_tx, first_rx) = mpsc::channel();
    let (rival_tx, rival_rx) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let result = download(
                &file_request(source.url("/file").replace("127.0.0.1", "localhost")),
                &output,
                &TransferControl::default(),
                &network,
                2,
                None,
                &mut |progress| {
                    if progress.downloaded > 0 {
                        let _ = first_tx.send(());
                    }
                },
            );
            done_tx.send(result).unwrap();
        });
        // The other job starts as soon as the download reports its first bytes, while it holds
        // both slots, so it has to wait for one.
        first_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the download reported no bytes");
        scope.spawn(|| {
            let result = download(
                &file_request(rival.url("/rival.bin")),
                &rival_output,
                &rival_control,
                &network,
                1,
                None,
                &mut |_| {},
            );
            rival_tx.send(result.is_ok()).unwrap();
        });
        let outcome = done_rx.recv_timeout(Duration::from_secs(25));
        let rival_ended = rival_rx.try_recv().is_ok();
        rival_control.cancel();
        let result = outcome
            .expect("the download never ended")
            .unwrap_or_else(|error| panic!("{error:#}"));
        assert_file_is(&result.path, &pattern(1, size), "rival");
        assert!(!rival_ended, "the download waited for the other job to end");
    });
}

/// A text file served at `path`.
fn text_document(path: &str, body: &str) -> Document {
    Document {
        path: path.into(),
        status: 200,
        headers: vec!["Content-Type: text/plain".into()],
        body: body.as_bytes().to_vec(),
        unframed: false,
    }
}

/// An answer of `status` that holds no digest, as a server gives for a file it does not have.
fn answer_without_digest(path: &str, status: u16, headers: Vec<String>) -> Document {
    Document {
        path: path.into(),
        status,
        headers,
        body: b"no".to_vec(),
        unframed: false,
    }
}

fn missing(path: &str) -> Document {
    answer_without_digest(path, 404, Vec::new())
}

/// A server for `payload.bin` (body version 1) that serves the digest files in `documents`.
fn serving(size: u64, documents: Vec<Document>) -> Server {
    Server::start(Config {
        size,
        documents,
        ..Config::default()
    })
}

/// A server whose file is body version 1 for its first pass and body version 2 from its second
/// `HEAD` on, and whose `Repr-Digest` states the digest of body version 2: the first pass fails
/// the digest, the second one meets it.
fn corrupted_once(size: u64, documents: Vec<Document>) -> Server {
    Server::start(Config {
        size,
        extra_headers: vec![repr_digest_header(2, size)],
        switch_version_at_head: Some(1),
        documents,
        ..Config::default()
    })
}

/// A server whose file is body version 1 for its first pass and body version 2 from its second
/// `HEAD` on, and whose digests state body version 3: no pass meets them, and the passes differ
/// from each other. The digest is a `Repr-Digest` header when `header` is set, else the digests
/// are in `documents`.
fn corrupted_differently(size: u64, header: bool, documents: Vec<Document>) -> Server {
    Server::start(Config {
        size,
        extra_headers: if header {
            vec![repr_digest_header(3, size)]
        } else {
            Vec::new()
        },
        switch_version_at_head: Some(1),
        documents,
        ..Config::default()
    })
}

fn forget_verified() {
    TEST_VERIFIED.with(|verified| verified.borrow_mut().clear());
}

fn verified_by() -> Vec<digest::Source> {
    TEST_VERIFIED.with(|verified| verified.borrow().clone())
}

fn lookup_paths(server: &Server) -> Vec<String> {
    server
        .lookups()
        .into_iter()
        .map(|entry| entry.path)
        .collect()
}

/// The names in `directory`: a download that failed leaves none, one that succeeded only the
/// file, whatever it kept beside it while it ran.
fn left_in(directory: &TempDir) -> Vec<String> {
    let mut names = fs::read_dir(&directory.0)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn is_trf_003(error: &anyhow::Error) -> bool {
    crate::error_codes::code_of(error) == Some(crate::error_codes::TRF_003)
}

fn heads(server: &Server) -> usize {
    server
        .log()
        .iter()
        .filter(|entry| entry.method == "HEAD")
        .count()
}

/// Downloads `payload.bin` from `server` into a fresh directory.
fn fetch_payload(server: &Server, connections: u8) -> (Result<TransferResult>, TempDir) {
    let directory = TempDir::new("digest");
    let output = directory.0.join("payload.bin");
    let result = fetch(
        &request_for(server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |_| {},
    );
    (result, directory)
}

fn assert_published(result: Result<TransferResult>, directory: &TempDir, version: u8, size: u64) {
    let result = result.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(file_sha256(&result.path), pattern_sha256(version, size));
    assert_eq!(left_in(directory), ["payload.bin"]);
}

fn assert_refused(result: Result<TransferResult>, directory: &TempDir) -> anyhow::Error {
    let error = result.expect_err("the file should not have been published");
    assert!(is_trf_003(&error), "{error:#}");
    assert_eq!(left_in(directory), Vec::<String>::new());
    error
}

fn repr_digest_verifies(connections: u8, size: u64) {
    let server = Server::start(Config {
        size,
        extra_headers: vec![repr_digest_header(1, size)],
        logged_headers: vec!["accept-encoding".into(), "want-repr-digest".into()],
        ..Config::default()
    });
    forget_verified();
    let (result, directory) = fetch_payload(&server, connections);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::ReprDigest]);
    // The digest costs no request, and the engine asks for neither a coding nor a digest.
    assert_eq!(server.lookups().len(), 0);
    for entry in server.requests() {
        assert_eq!(entry.header("accept-encoding"), None, "{entry:?}");
        assert_eq!(entry.header("want-repr-digest"), None, "{entry:?}");
    }
    assert_eq!(
        ranged_gets(&server.log(), size).is_empty(),
        connections == 1
    );
}

#[test]
fn a_matching_repr_digest_verifies_a_one_connection_download() {
    repr_digest_verifies(1, 3 * MIB + 5);
}

#[test]
fn a_matching_repr_digest_verifies_a_download_over_several_connections() {
    repr_digest_verifies(4, 9 * MIB + 5);
}

#[test]
fn a_repr_digest_verifies_a_file_of_any_size() {
    repr_digest_verifies(1, 100_000);
}

#[test]
fn a_repr_digest_that_only_the_get_answer_carries_verifies_the_file() {
    for (connections, size) in [(1, 3 * MIB + 5), (4, 9 * MIB + 5)] {
        let server = Server::start(Config {
            size,
            head_bare: true,
            extra_headers: vec![repr_digest_header(1, size)],
            ..Config::default()
        });
        forget_verified();
        let (result, directory) = fetch_payload(&server, connections);
        assert_published(result, &directory, 1, size);
        assert_eq!(verified_by(), [digest::Source::ReprDigest], "{connections}");
    }
}

#[test]
fn a_repr_digest_describes_the_stored_bytes_whatever_content_coding_the_server_names() {
    // The digest of a representation covers the bytes as sent, with their coding applied, and
    // the engine stores exactly those bytes.
    let size = 3 * MIB + 5;
    let server = Server::start(Config {
        size,
        extra_headers: vec!["Content-Encoding: gzip".into(), repr_digest_header(1, size)],
        ..Config::default()
    });
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::ReprDigest]);
}

#[test]
fn a_repr_digest_that_covers_a_resumed_download_is_the_digest_of_the_whole_file() {
    let size = 3 * MIB + 5;
    let server = Server::start(Config {
        size,
        extra_headers: vec![repr_digest_header(1, size)],
        cut_body: Some(CutBody {
            after_bytes: MIB,
            responses: 1,
        }),
        ..Config::default()
    });
    let directory = TempDir::new("digest-resumed");
    let output = directory.0.join("payload.bin");
    let request = request_for(&server);
    let control = TransferControl::default();
    fetch(&request, &output, &control, 1, 1, &mut |_| {}).unwrap_err();
    assert!(retained_bytes(&output).unwrap() > 0);

    forget_verified();
    let result = fetch(&request, &output, &control, 1, 1, &mut |_| {}).unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert_eq!(verified_by(), [digest::Source::ReprDigest]);
    let ranged = server
        .log()
        .into_iter()
        .filter(|entry| entry.status == 206)
        .count();
    assert_eq!(ranged, 1);
}

fn mismatch_then_match(connections: u8, size: u64) {
    let server = corrupted_once(size, Vec::new());
    let directory = TempDir::new("digest-again");
    let output = directory.0.join("payload.bin");
    let mut phases = Vec::new();
    let control = TransferControl::default();
    forget_verified();
    let result = fetch(
        &request_for(&server),
        &output,
        &control,
        connections,
        connections,
        &mut |progress| phases.push(progress.phase),
    );
    assert_published(result, &directory, 2, size);
    assert_eq!(verified_by(), [digest::Source::ReprDigest]);
    assert_eq!(heads(&server), 2);
    // Bytes that were discarded and fetched again are no progress for the engine.
    assert!(control.restarted());
    // The whole body was fetched twice, and a mismatch is announced once.
    assert!(server.body_bytes_sent() >= 2 * size);
    assert!(server.body_bytes_sent() < 3 * size);
    let notice = DigestRetry(0).to_string();
    assert_eq!(phases.iter().filter(|phase| **phase == notice).count(), 1);
}

#[test]
fn a_repr_digest_mismatch_is_followed_by_one_download_from_scratch_that_is_published() {
    mismatch_then_match(1, 3 * MIB + 5);
}

#[test]
fn a_repr_digest_mismatch_over_several_connections_is_followed_by_one_download_from_scratch() {
    mismatch_then_match(4, 9 * MIB + 5);
}

fn mismatch_twice(connections: u8, size: u64) {
    let server = corrupted_differently(size, true, Vec::new());
    forget_verified();
    let (result, directory) = fetch_payload(&server, connections);
    let error = assert_refused(result, &directory);
    assert!(format!("{error:#}").contains("Repr-Digest"), "{error:#}");
    assert_eq!(heads(&server), 2);
    assert!(server.body_bytes_sent() >= 2 * size);
    assert!(server.body_bytes_sent() < 3 * size);
    assert_eq!(verified_by(), []);

    // The used re-download is not carried over to a retry of the failed job: it starts anew, and
    // gets a re-download of its own. The server now serves body version 2 throughout, so the
    // two passes agree and the file stays.
    let output = directory.0.join("payload.bin");
    let again = fetch(
        &request_for(&server),
        &output,
        &TransferControl::default(),
        connections,
        connections,
        &mut |_| {},
    );
    assert_kept_with_a_warning(again, &directory, "Repr-Digest", 2, size);
    assert_eq!(heads(&server), 4);
}

#[test]
fn two_downloads_that_differ_and_miss_the_digest_fail_with_trf_003_and_publish_nothing() {
    mismatch_twice(1, 3 * MIB + 5);
}

#[test]
fn two_downloads_over_several_connections_that_differ_and_miss_the_digest_fail_with_trf_003() {
    mismatch_twice(4, 9 * MIB + 5);
}

/// A server that always serves body version 1 and publishes the digest of body version 2: a stale
/// digest, as when the publisher replaced the file and not its checksum.
fn stale_digest(size: u64, header: bool, documents: Vec<Document>) -> Server {
    Server::start(Config {
        size,
        extra_headers: if header {
            vec![repr_digest_header(2, size)]
        } else {
            Vec::new()
        },
        documents,
        ..Config::default()
    })
}

/// The file of a download whose two passes gave the same bytes of body `version`, not the
/// digest's: published, with a note on the job that names `source`.
fn assert_kept_with_a_warning(
    result: Result<TransferResult>,
    directory: &TempDir,
    source: &str,
    version: u8,
    size: u64,
) {
    let result = result.unwrap_or_else(|error| panic!("{error:#}"));
    assert_eq!(file_sha256(&result.path), pattern_sha256(version, size));
    assert_eq!(left_in(directory), ["payload.bin"]);
    let warning = result.warning.expect("the job gets a note");
    assert!(warning.contains(source), "{warning}");
    assert_eq!(verified_by(), []);
}

fn stale_repr_digest(connections: u8, size: u64) {
    let server = stale_digest(size, true, Vec::new());
    forget_verified();
    let (result, directory) = fetch_payload(&server, connections);
    assert_kept_with_a_warning(result, &directory, "Repr-Digest", 1, size);
    // The file was fetched twice, and the second copy is the one that stays.
    assert_eq!(heads(&server), 2);
    assert!(server.body_bytes_sent() >= 2 * size);
    assert!(server.body_bytes_sent() < 3 * size);
}

#[test]
fn a_stale_repr_digest_that_two_identical_downloads_do_not_meet_keeps_the_file_with_a_warning() {
    stale_repr_digest(1, 3 * MIB + 5);
}

#[test]
fn a_stale_repr_digest_over_several_connections_keeps_the_file_with_a_warning() {
    stale_repr_digest(4, 9 * MIB + 5);
}

#[test]
fn a_restart_between_the_first_mismatch_and_the_second_download_allows_no_third() {
    let size = 3 * MIB + 5;
    let server = corrupted_differently(size, true, Vec::new());
    let directory = TempDir::new("digest-restart");
    let output = directory.0.join("payload.bin");
    let request = request_for(&server);

    // The app closes right after the first mismatch is acted on.
    let control = TransferControl::default();
    let closing = control.clone();
    let notice = DigestRetry(0).to_string();
    let error = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.phase == notice {
            closing.pause();
        }
    })
    .unwrap_err();
    assert!(!is_trf_003(&error), "{error:#}");
    assert_eq!(heads(&server), 1);
    assert!(!part_path(&output).exists() && !state_path(&output).exists());
    let record = digest::retry_record(&state_path(&output));
    assert_eq!(
        digest::first_download(&record),
        Some(pattern_sha256(1, size)),
        "the first download is not recorded"
    );

    // The next start downloads once more, finds other bytes that miss the digest and stops there.
    let error = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(is_trf_003(&error), "{error:#}");
    assert_eq!(heads(&server), 2);
    assert_eq!(server.body_bytes_sent(), 2 * size);
    assert_eq!(left_in(&directory), Vec::<String>::new());
}

#[test]
fn a_restart_between_the_first_mismatch_and_a_second_download_of_the_same_bytes_keeps_the_file() {
    let size = 3 * MIB + 5;
    let server = stale_digest(size, true, Vec::new());
    let directory = TempDir::new("digest-restart-same");
    let output = directory.0.join("payload.bin");
    let request = request_for(&server);

    let control = TransferControl::default();
    let closing = control.clone();
    let notice = DigestRetry(0).to_string();
    fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.phase == notice {
            closing.pause();
        }
    })
    .unwrap_err();

    forget_verified();
    let result = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    );
    assert_kept_with_a_warning(result, &directory, "Repr-Digest", 1, size);
    assert_eq!(heads(&server), 2);
    assert_eq!(server.body_bytes_sent(), 2 * size);
}

#[test]
fn a_sha256_file_next_to_the_address_verifies_the_file() {
    let size = 2 * MIB + 11;
    let digest = pattern_sha256(1, size);
    let upper = digest.to_ascii_uppercase();
    for body in [
        format!("{digest}\n"),
        format!("{digest}  payload.bin\n"),
        format!("{digest} *payload.bin\r\n"),
        format!("\n{upper}  payload.bin\n\n"),
    ] {
        let server = serving(
            size,
            vec![
                text_document("/payload.bin.sha256", &body),
                missing("/SHA256SUMS"),
            ],
        );
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        assert_eq!(verified_by(), [digest::Source::Sha256File], "{body:?}");
        // The first source that answers cleanly ends the lookup.
        assert_eq!(lookup_paths(&server), ["/payload.bin.sha256"], "{body:?}");
    }
}

#[test]
fn a_stale_sha256_file_that_two_identical_downloads_do_not_meet_keeps_the_file_with_a_warning() {
    let size = 2 * MIB + 11;
    let stale = pattern_sha256(2, size);
    let server = stale_digest(
        size,
        false,
        vec![
            text_document("/payload.bin.sha256", &format!("{stale}  payload.bin\n")),
            missing("/SHA256SUMS"),
        ],
    );
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_kept_with_a_warning(result, &directory, ".sha256", 1, size);
    assert_eq!(heads(&server), 2);
    // Each of the two downloads asked once.
    assert_eq!(lookup_paths(&server), ["/payload.bin.sha256"; 2]);
}

#[test]
fn a_sha256_file_that_two_different_downloads_do_not_meet_fails_the_download() {
    let size = 2 * MIB + 11;
    let wrong = pattern_sha256(3, size);
    let server = corrupted_differently(
        size,
        false,
        vec![
            text_document("/payload.bin.sha256", &format!("{wrong}  payload.bin\n")),
            missing("/SHA256SUMS"),
        ],
    );
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    let error = assert_refused(result, &directory);
    assert!(format!("{error:#}").contains(".sha256"), "{error:#}");
    assert_eq!(heads(&server), 2);
    assert_eq!(verified_by(), []);
    assert_eq!(lookup_paths(&server), ["/payload.bin.sha256"; 2]);
}

#[test]
fn a_sha256_file_that_matches_the_second_download_publishes_it() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(2, size);
    let server = Server::start(Config {
        size,
        switch_version_at_head: Some(1),
        documents: vec![
            text_document("/payload.bin.sha256", &format!("{right}\n")),
            missing("/SHA256SUMS"),
        ],
        ..Config::default()
    });
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 2, size);
    assert_eq!(verified_by(), [digest::Source::Sha256File]);
    assert_eq!(heads(&server), 2);
}

#[test]
fn sha256sums_verifies_the_file_by_the_line_that_names_it() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let other = "0".repeat(64);
    let sums = format!(
        "{other}  payload.bin.old\n{right}  payload.bin\n{other} *xpayload.bin\n{other}  payload.bin2\n"
    );
    let server = serving(
        size,
        vec![
            missing("/payload.bin.sha256"),
            text_document("/SHA256SUMS", &sums),
        ],
    );
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::Sha256Sums]);
    assert_eq!(
        lookup_paths(&server),
        ["/payload.bin.sha256", "/SHA256SUMS"]
    );

    let documents = |version| {
        let line = format!("{}  payload.bin", pattern_sha256(version, size));
        vec![
            missing("/payload.bin.sha256"),
            text_document(
                "/SHA256SUMS",
                &format!("{other}  payload.bin.old\n{line}\n"),
            ),
        ]
    };
    // A stale line, with both downloads alike: the file stays, with a note.
    let server = stale_digest(size, false, documents(2));
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_kept_with_a_warning(result, &directory, "SHA256SUMS", 1, size);
    assert_eq!(heads(&server), 2);

    // A line that neither download meets, with downloads that differ: the job fails.
    let server = corrupted_differently(size, false, documents(3));
    let (result, directory) = fetch_payload(&server, 1);
    let error = assert_refused(result, &directory);
    assert!(format!("{error:#}").contains("SHA256SUMS"), "{error:#}");
    assert_eq!(heads(&server), 2);
}

#[test]
fn a_digest_file_that_is_not_a_clean_match_leaves_the_file_unverified() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let wrong = pattern_sha256(2, size);
    let page = Document {
        headers: vec!["Content-Type: text/html".into()],
        ..text_document("/payload.bin.sha256", &format!("{wrong}\n"))
    };
    let unclean = [
        // A name other than the file's, a name that only contains it, several candidates.
        (
            text_document("/payload.bin.sha256", &format!("{right}  other.bin\n")),
            missing("/SHA256SUMS"),
        ),
        (
            text_document(
                "/payload.bin.sha256",
                &format!("{right}  payload.bin.old\n"),
            ),
            missing("/SHA256SUMS"),
        ),
        (
            text_document(
                "/payload.bin.sha256",
                &format!("{wrong}  payload.bin\n{right}  payload.bin\n"),
            ),
            missing("/SHA256SUMS"),
        ),
        // Malformed digests and an empty file.
        (
            text_document("/payload.bin.sha256", &format!("{}\n", &right[1..])),
            missing("/SHA256SUMS"),
        ),
        (
            text_document("/payload.bin.sha256", &format!("{}z\n", &right[1..])),
            missing("/SHA256SUMS"),
        ),
        (
            text_document("/payload.bin.sha256", ""),
            missing("/SHA256SUMS"),
        ),
        // A web page, however it is spelled, and answers that are not a file.
        (page, missing("/SHA256SUMS")),
        (
            answer_without_digest("/payload.bin.sha256", 403, Vec::new()),
            answer_without_digest("/SHA256SUMS", 500, Vec::new()),
        ),
        // No line of SHA256SUMS names the file, or two of them do.
        (
            missing("/payload.bin.sha256"),
            text_document(
                "/SHA256SUMS",
                &format!("{right}  other.bin\n{wrong}  Payload.bin\n"),
            ),
        ),
        (
            missing("/payload.bin.sha256"),
            text_document(
                "/SHA256SUMS",
                &format!("{wrong}  payload.bin\n{right}  payload.bin\n"),
            ),
        ),
        (
            missing("/payload.bin.sha256"),
            text_document("/SHA256SUMS", &format!("{right}\n")),
        ),
    ];
    for (index, (file, sums)) in unclean.into_iter().enumerate() {
        let server = serving(size, vec![file, sums]);
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        assert_eq!(verified_by(), [], "case {index}");
        assert_eq!(lookup_paths(&server).len(), 2, "case {index}");
        assert_eq!(heads(&server), 1);
    }
}

#[test]
fn a_catch_all_server_that_answers_digest_addresses_with_the_file_itself_is_ignored() {
    let size = 2 * MIB + 11;
    for unframed in [false, true] {
        let server = Server::start(Config {
            size,
            get_length_header: !unframed,
            ..Config::default()
        });
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        assert_eq!(verified_by(), []);
        assert_eq!(lookup_paths(&server).len(), 2);
    }
}

#[test]
fn a_digest_file_is_read_up_to_64_kib_and_not_beyond() {
    let size = 2 * MIB + 11;
    let line = format!("{}  payload.bin\n", pattern_sha256(1, size));
    let cap = 64 * 1024;
    let padded = |length: usize| format!("{line}{}", "\n".repeat(length - line.len()));
    for (body, unframed, accepted) in [
        (padded(cap), false, true),
        (padded(cap), true, true),
        (padded(cap + 1), false, false),
        (padded(cap + 1), true, false),
        (padded(4 * cap), false, false),
        (padded(4 * cap), true, false),
    ] {
        let sha256 = Document {
            unframed,
            ..text_document("/payload.bin.sha256", &body)
        };
        let server = serving(size, vec![sha256, missing("/SHA256SUMS")]);
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        let expected: &[digest::Source] = if accepted {
            &[digest::Source::Sha256File]
        } else {
            &[]
        };
        assert_eq!(
            verified_by(),
            expected,
            "{} bytes, unframed {unframed}",
            body.len()
        );
    }
}

/// The requests that are no part of the file's own traffic: for its digest files and what they
/// redirect to.
fn extra_requests(server: &Server) -> usize {
    server
        .requests()
        .iter()
        .filter(|entry| entry.path != "/payload.bin")
        .count()
}

#[test]
fn a_download_asks_for_at_most_two_digest_files_even_through_redirects() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let redirect = |to: &str| vec![format!("Location: {to}")];
    let chain = vec![
        answer_without_digest("/payload.bin.sha256", 302, redirect("/a")),
        answer_without_digest("/a", 302, redirect("/b")),
        text_document("/b", &format!("{right}\n")),
        text_document("/SHA256SUMS", &format!("{right}  payload.bin\n")),
    ];
    let server = serving(size, chain);
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    // The chain needs a third request for the digest, and nothing else is tried.
    assert_eq!(verified_by(), []);
    let requests = server.requests();
    let paths = requests
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        ["/payload.bin", "/payload.bin", "/payload.bin.sha256", "/a"]
    );
    assert_eq!(extra_requests(&server), 2);

    let sums_after_a_failure = vec![
        missing("/payload.bin.sha256"),
        text_document("/SHA256SUMS", &format!("{right}  payload.bin\n")),
    ];
    let server = serving(size, sums_after_a_failure);
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::Sha256Sums]);
    assert_eq!(extra_requests(&server), 2);
}

#[test]
fn a_digest_file_behind_a_redirect_inside_the_origin_is_read() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let server = serving(
        size,
        vec![
            answer_without_digest(
                "/payload.bin.sha256",
                302,
                vec!["Location: /checksums/payload.txt".into()],
            ),
            text_document("/checksums/payload.txt", &format!("{right}  payload.bin\n")),
        ],
    );
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::Sha256File]);
    assert_eq!(extra_requests(&server), 2);
    assert_eq!(
        server.requests().last().map(|entry| entry.path.as_str()),
        Some("/checksums/payload.txt")
    );
}

#[test]
fn digest_files_are_read_from_the_origin_of_the_address_given_never_from_a_redirect_target() {
    let size = 2 * MIB + 11;
    let wrong = pattern_sha256(2, size);
    // The file lives on `target`, which also publishes digests, wrong ones: they must not be
    // read, whatever the address that redirected to them is.
    let target = serving(
        size,
        vec![
            text_document("/payload.bin.sha256", &format!("{wrong}\n")),
            text_document("/SHA256SUMS", &format!("{wrong}  payload.bin\n")),
        ],
    );
    let source = Server::start(Config {
        redirect_to: Some(target.url("/payload.bin")),
        ..Config::default()
    });
    forget_verified();
    let (result, directory) = fetch_payload(&source, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), []);
    assert_eq!(
        lookup_paths(&source),
        ["/payload.bin.sha256", "/SHA256SUMS"]
    );
    // The redirects of the digest requests lead to the target, which is not followed.
    assert_eq!(target.lookups().len(), 0);
    assert!(target
        .requests()
        .iter()
        .all(|entry| entry.path == "/payload.bin"));

    // A digest file that redirects to another origin is not followed either.
    let elsewhere = serving(size, Vec::new());
    let redirect = |path: &str| {
        answer_without_digest(
            path,
            302,
            vec![format!(
                "Location: {}",
                elsewhere.url("/payload.bin.sha256")
            )],
        )
    };
    let source = serving(
        size,
        vec![redirect("/payload.bin.sha256"), redirect("/SHA256SUMS")],
    );
    let (result, directory) = fetch_payload(&source, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(lookup_paths(&source).len(), 2);
    assert_eq!(elsewhere.requests().len(), 0);
}

#[test]
fn digest_file_requests_carry_the_headers_of_the_download_to_its_origin_only() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let server = Server::start(Config {
        size,
        documents: vec![
            missing("/payload.bin.sha256"),
            text_document("/SHA256SUMS", &format!("{right}  payload.bin\n")),
        ],
        logged_headers: vec!["authorization".into(), "cookie".into(), "referer".into()],
        ..Config::default()
    });
    let mut request = request_for(&server);
    request
        .headers
        .insert("Authorization".into(), "Bearer token".into());
    request.referer = Some("https://page.example/".into());
    request.session_cookies = vec![crate::model::ScopedCookie {
        name: "session".into(),
        value: "abc".into(),
        domain: "127.0.0.1".into(),
        path: "/".into(),
        host_only: true,
        ..Default::default()
    }];
    let directory = TempDir::new("digest-headers");
    let output = directory.0.join("payload.bin");
    forget_verified();
    fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(verified_by(), [digest::Source::Sha256Sums]);

    let download = server
        .log()
        .into_iter()
        .find(|entry| entry.method == "GET")
        .unwrap();
    assert_eq!(download.header("authorization"), Some("Bearer token"));
    assert_eq!(download.header("cookie"), Some("session=abc"));
    let lookups = server.lookups();
    assert_eq!(lookups.len(), 2);
    for lookup in lookups {
        assert_eq!(lookup.headers, download.headers, "{lookup:?}");
    }
}

#[test]
fn a_user_checksum_decides_alone_and_asks_for_nothing_else() {
    let size = 2 * MIB + 11;
    let wrong = pattern_sha256(2, size);
    let published_wrong = |size| {
        Server::start(Config {
            size,
            extra_headers: vec![repr_digest_header(2, size)],
            documents: vec![
                text_document("/payload.bin.sha256", &format!("{wrong}\n")),
                text_document("/SHA256SUMS", &format!("{wrong}  payload.bin\n")),
            ],
            ..Config::default()
        })
    };

    // The user's digest matches, the source's do not: the file is published.
    let server = published_wrong(size);
    let mut request = request_for(&server);
    request.checksum = Some(pattern_sha256(1, size));
    let directory = TempDir::new("digest-user");
    let output = directory.0.join("payload.bin");
    forget_verified();
    fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&output), pattern_sha256(1, size));
    assert_eq!(verified_by(), []);
    assert_eq!(server.lookups().len(), 0);

    // The user's digest does not match, the source's do not either: one download, then the
    // error the user's digest always gave.
    let server = published_wrong(size);
    let mut request = request_for(&server);
    request.checksum = Some("0".repeat(64));
    let directory = TempDir::new("digest-user-wrong");
    let output = directory.0.join("payload.bin");
    let error = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(is_trf_003(&error), "{error:#}");
    assert_eq!(heads(&server), 1);
    assert_eq!(server.lookups().len(), 0);
    assert_eq!(left_in(&directory), Vec::<String>::new());

    // The source's digest is right and the user's is wrong: the user's still decides.
    let server = Server::start(Config {
        size,
        extra_headers: vec![repr_digest_header(1, size)],
        ..Config::default()
    });
    let mut request = request_for(&server);
    request.checksum = Some(wrong);
    let error = fetch(
        &request,
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(is_trf_003(&error), "{error:#}");
    assert!(!output.exists());
}

#[test]
fn digest_files_are_only_looked_up_for_files_of_at_least_one_mebibyte() {
    let right = |size| pattern_sha256(1, size);
    for (size, looked_up) in [(MIB - 1, false), (MIB, true), (2 * MIB, true)] {
        let server = serving(
            size,
            vec![
                text_document("/payload.bin.sha256", &format!("{}\n", right(size))),
                missing("/SHA256SUMS"),
            ],
        );
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        assert_eq!(server.lookups().len(), usize::from(looked_up), "{size}");
        assert_eq!(
            verified_by(),
            if looked_up {
                vec![digest::Source::Sha256File]
            } else {
                Vec::new()
            },
            "{size}"
        );
    }
}

#[test]
fn a_file_without_any_published_digest_is_published_as_it_is() {
    let size = 3 * MIB + 5;
    let server = serving(
        size,
        vec![missing("/payload.bin.sha256"), missing("/SHA256SUMS")],
    );
    forget_verified();
    let (result, directory) = fetch_payload(&server, 1);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), []);
    assert_eq!(heads(&server), 1);
}

#[test]
fn a_download_stopped_when_it_is_complete_asks_for_no_digest_file_and_is_not_published() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let server = serving(
        size,
        vec![
            text_document("/payload.bin.sha256", &format!("{right}\n")),
            missing("/SHA256SUMS"),
        ],
    );
    let directory = TempDir::new("digest-stopped");
    let output = directory.0.join("payload.bin");
    let request = request_for(&server);
    let control = TransferControl::default();
    let pausing = control.clone();
    let error = fetch(&request, &output, &control, 1, 1, &mut |progress| {
        if progress.downloaded == size {
            pausing.pause();
        }
    })
    .unwrap_err();
    assert!(!is_trf_003(&error), "{error:#}");
    assert!(!output.exists());
    assert_eq!(server.lookups().len(), 0);
    assert!(part_path(&output).exists());

    // Resumed, the complete file is verified and published.
    forget_verified();
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
    assert_eq!(verified_by(), [digest::Source::Sha256File]);
}

#[test]
fn the_digest_files_of_an_address_with_an_encoded_name_and_a_query_are_found() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    let server = serving(
        size,
        vec![
            missing("/dir/my%20file.bin.sha256"),
            text_document("/dir/SHA256SUMS", &format!("{right}  my file.bin\n")),
        ],
    );
    let directory = TempDir::new("digest-name");
    let output = directory.0.join("my file.bin");
    forget_verified();
    let result = fetch(
        &file_request(server.url("/dir/my%20file.bin?download=1#top")),
        &output,
        &TransferControl::default(),
        1,
        1,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(file_sha256(&result.path), pattern_sha256(1, size));
    assert_eq!(verified_by(), [digest::Source::Sha256Sums]);
    assert_eq!(
        lookup_paths(&server),
        ["/dir/my%20file.bin.sha256", "/dir/SHA256SUMS"]
    );
}

/// Runs `run` with the whole digest lookup limited to `limit`.
fn with_lookup_limit<T>(limit: Duration, run: impl FnOnce() -> T) -> T {
    digest::TEST_LOOKUP_LIMIT.with(|cell| cell.set(Some(limit)));
    let result = run();
    digest::TEST_LOOKUP_LIMIT.with(|cell| cell.set(None));
    result
}

/// A server for `payload.bin` that publishes the right `.sha256` and answers digest files only
/// after `delay`.
fn slow_lookups(size: u64, delay: Duration) -> Server {
    let right = pattern_sha256(1, size);
    Server::start(Config {
        size,
        lookup_delay: Some(delay),
        documents: vec![
            text_document("/payload.bin.sha256", &format!("{right}\n")),
            missing("/SHA256SUMS"),
        ],
        ..Config::default()
    })
}

#[test]
fn a_repr_digest_that_only_the_head_answer_carries_survives_the_range_probe() {
    let size = 9 * MIB + 5;
    let server = Server::start(Config {
        size,
        head_extra_headers: vec![repr_digest_header(1, size)],
        ..Config::default()
    });
    forget_verified();
    let (result, directory) = fetch_payload(&server, 4);
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), [digest::Source::ReprDigest]);
    // The one-byte probe of the ranges was answered without the digest.
    assert!(server
        .log()
        .iter()
        .any(|entry| entry.range.as_deref() == Some("bytes=0-0")));
}

#[test]
fn a_digest_file_is_read_only_from_a_200_answer() {
    let size = 2 * MIB + 11;
    let right = pattern_sha256(1, size);
    for status in [201, 202, 203, 206, 403, 404, 410, 500, 503] {
        let answer = |path: &str, body: String| Document {
            status,
            ..text_document(path, &body)
        };
        let server = serving(
            size,
            vec![
                answer("/payload.bin.sha256", format!("{right}\n")),
                answer("/SHA256SUMS", format!("{right}  payload.bin\n")),
            ],
        );
        forget_verified();
        let (result, directory) = fetch_payload(&server, 1);
        assert_published(result, &directory, 1, size);
        assert_eq!(verified_by(), [], "status {status}");
        assert_eq!(lookup_paths(&server).len(), 2, "status {status}");
    }
}

#[test]
fn a_digest_lookup_that_outlasts_its_limit_is_dropped_and_the_file_published_unverified() {
    let size = 2 * MIB + 11;
    let server = slow_lookups(size, Duration::from_secs(5));
    forget_verified();
    let started = Instant::now();
    let (result, directory) =
        with_lookup_limit(Duration::from_secs(1), || fetch_payload(&server, 1));
    let waited = started.elapsed();
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), []);
    assert!(waited < Duration::from_secs(3), "{waited:?}");
}

#[test]
fn a_lookup_limit_under_a_millisecond_sends_no_lookup() {
    let size = 2 * MIB + 11;
    let server = slow_lookups(size, Duration::from_secs(5));
    forget_verified();
    let started = Instant::now();
    let (result, directory) =
        with_lookup_limit(Duration::from_micros(999), || fetch_payload(&server, 1));
    let waited = started.elapsed();
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), []);
    assert_eq!(lookup_paths(&server), Vec::<String>::new());
    assert!(waited < Duration::from_secs(3), "{waited:?}");
}

#[test]
fn a_stop_during_a_digest_lookup_ends_the_download_at_once_and_publishes_nothing() {
    let size = 2 * MIB + 11;
    let server = slow_lookups(size, Duration::from_secs(5));
    let directory = TempDir::new("digest-stop-lookup");
    let output = directory.0.join("payload.bin");
    let request = request_for(&server);
    let control = TransferControl::default();
    forget_verified();
    let started = Instant::now();
    let error = thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..2000 {
                if !server.lookups().is_empty() {
                    control.pause();
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        fetch(&request, &output, &control, 1, 1, &mut |_| {}).unwrap_err()
    });
    let waited = started.elapsed();
    assert!(!is_trf_003(&error), "{error:#}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert!(!output.exists());
    assert!(part_path(&output).exists());
    assert_eq!(verified_by(), []);
}

#[test]
fn a_download_whose_host_has_no_free_connection_for_a_lookup_is_published_unverified() {
    let size = 4 * MIB + 11;
    let right = pattern_sha256(1, size);
    let server = serving(
        size,
        vec![
            text_document("/payload.bin.sha256", &format!("{right}\n")),
            missing("/SHA256SUMS"),
        ],
    );
    let directory = TempDir::new("digest-no-slot");
    let output = directory.0.join("payload.bin");
    let governor = NetworkGovernor::new(2);
    // Other downloads take every connection of the host the moment this one has its bytes, and
    // keep them for longer than the test lets anything wait.
    let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let releasing = held.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(12));
        releasing.lock().unwrap().clear();
    });
    let completing = crate::i18n::ui("Tamamlanıyor", "Completing");
    forget_verified();
    let started = Instant::now();
    let result = with_lookup_limit(Duration::from_secs(1), || {
        download(
            &request_for(&server),
            &output,
            &TransferControl::default(),
            &governor,
            2,
            None,
            &mut |progress| {
                if progress.phase == completing {
                    let mut held = held.lock().unwrap();
                    while let Some(lease) = governor.try_acquire_host("127.0.0.1".into()) {
                        held.push(lease);
                    }
                }
            },
        )
    });
    let waited = started.elapsed();
    assert!(!held.lock().unwrap().is_empty(), "no slot was taken");
    assert_published(result, &directory, 1, size);
    assert_eq!(verified_by(), []);
    assert_eq!(server.lookups().len(), 0);
    assert!(waited < Duration::from_secs(6), "{waited:?}");
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
