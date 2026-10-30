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
    server.wait_idle();
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
    server.wait_idle();
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
    let ranged = ranged_gets(&log, size);
    assert_tiles(ranged.clone(), size);
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
            server.wait_idle();
            assert_tiles(ranged_gets(&server.log(), size), size);
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
    assert_eq!(log[0].method, "HEAD");
    assert_eq!(log[0].status, 405);
    assert_tiles(ranged_gets(&log, size), size);
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
    let probe = log
        .iter()
        .find(|entry| entry.range.as_deref() == Some("bytes=0-0"))
        .unwrap();
    assert_eq!((probe.if_range.as_deref(), probe.status), (None, 206));
    assert_tiles(ranged_gets(&log, size), size);
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
    assert_tiles(ranged_gets(&log, size), size);
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
    let interrupted = fetch(&request, &output, &control, 2, 2, &mut |_| {
        if read_state(&state_file).is_some_and(|state| !state.completed.is_empty()) {
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
    server.wait_idle();
    let log = server.log();
    let resumed = &log[first_attempt..];
    let mut requested = ranged_gets(resumed, size);
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
