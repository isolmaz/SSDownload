//! The check a downloaded tool set passes before it becomes the active one. FFmpeg generates a
//! video file and an audio file, yt-dlp downloads them from local `file:` addresses the way it
//! downloads a pinned bundle (the application's own arguments and marker lines), merges them
//! through FFmpeg, and ffprobe has to read the result. Nothing leaves the machine and the run
//! takes a few seconds.

use super::*;

const CLIP_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a tool to its end and returns what it printed.
type Run<'a> = dyn FnMut(&Path, &[String], Duration) -> Result<Captured> + 'a;

/// Runs the check with `tools` in `scratch`, a folder it creates and leaves to the caller.
pub(crate) fn self_test(
    tools: &tools::VerifiedTools,
    scratch: &Path,
    control: &TransferControl,
) -> Result<()> {
    self_test_with(tools, scratch, &mut |executable, args, timeout| {
        run_capture(executable, args, b"", timeout, control)
    })
}

fn self_test_with(tools: &tools::VerifiedTools, scratch: &Path, run: &mut Run) -> Result<()> {
    check_tools(tools, scratch, run).map_err(|error| {
        anyhow!(
            "{}",
            crate::i18n::ui_format!(
                "Medya özdenetimi başarısız: {0}",
                "The media self-test failed: {0}",
                format!("{error:#}")
            )
        )
    })
}

fn check_tools(tools: &tools::VerifiedTools, scratch: &Path, run: &mut Run) -> Result<()> {
    fs::create_dir_all(scratch)?;
    let video = scratch.join("video.mp4");
    let audio = scratch.join("audio.m4a");
    let made = run(&tools.ffmpeg, &clip_args(&video, &audio), CLIP_TIMEOUT)?;
    if !made.status.success() || !video.is_file() || !audio.is_file() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "FFmpeg deneme klibini üretemedi ({0}): {1}",
                "FFmpeg did not generate the test clip ({0}): {1}",
                describe(&made.status),
                bounded_tail(made.stderr.trim(), 400)
            )
        );
    }

    let info = scratch.join("info.json");
    fs::write(&info, serde_json::to_vec(&info_document(&video, &audio)?)?)?;
    let output = scratch.join("out");
    let mut args = common_args(&tools.deno, &tools.ffmpeg, false);
    args.extend(load_info_args(&info, true));
    args.extend(["--windows-filenames".into(), "--no-overwrites".into()]);
    args.extend(marker_args());
    args.extend([
        "--format".into(),
        "v+a".into(),
        "--merge-output-format".into(),
        "mkv".into(),
        "--output".into(),
        output.join("%(id)s.%(ext)s").to_string_lossy().into_owned(),
    ]);
    let downloaded = run(&tools.yt_dlp, &args, DOWNLOAD_TIMEOUT)?;
    let file = check_download(&downloaded)?;

    let probed = run(&tools.ffprobe, &probe_args(&file), PROBE_TIMEOUT)?;
    if !probed.status.success() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "ffprobe çıktıyı okuyamadı ({0})",
                "ffprobe could not read the output ({0})",
                describe(&probed.status)
            )
        );
    }
    let probe: Value = serde_json::from_slice(&probed.stdout).context(crate::i18n::ui(
        "ffprobe JSON yazdırmadı",
        "ffprobe did not print JSON",
    ))?;
    validate_probe(&probe, &probe_request(), &MediaInfo::default())
}

/// What the merged file has to be: a video with audio.
fn probe_request() -> AddRequest {
    AddRequest {
        kind: DownloadKind::Video,
        expected_audio: Some(true),
        ..AddRequest::default()
    }
}

fn clip_args(video: &Path, audio: &Path) -> Vec<String> {
    let mut args = [
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=160x90:rate=10:duration=1",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=1",
        "-map",
        "0:v",
        "-c:v",
        "mpeg4",
    ]
    .map(String::from)
    .to_vec();
    args.push(video.to_string_lossy().into_owned());
    args.extend(["-map", "1:a", "-c:a", "aac"].map(String::from));
    args.push(audio.to_string_lossy().into_owned());
    args
}

/// The extractor document yt-dlp downloads from: one video and one audio format, both local files.
fn info_document(video: &Path, audio: &Path) -> Result<Value> {
    let address = |path: &Path| {
        Url::from_file_path(path).map(String::from).map_err(|()| {
            anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "{0} bir dosya adresi değil",
                    "{0} is not a file address",
                    path.display()
                )
            )
        })
    };
    let (video, audio) = (address(video)?, address(audio)?);
    Ok(serde_json::json!({
        "id": "selftest",
        "title": "selftest",
        "ext": "mkv",
        "extractor": "generic",
        "extractor_key": "Generic",
        "webpage_url": video,
        "duration": 1,
        "formats": [
            {"format_id": "v", "url": video, "ext": "mp4", "vcodec": "mpeg4", "acodec": "none", "width": 160, "height": 90},
            {"format_id": "a", "url": audio, "ext": "m4a", "vcodec": "none", "acodec": "aac"},
        ],
    }))
}

/// The file yt-dlp reports, after the same reading of its marker lines as a real download:
/// progress and file markers come from stdout, post-processor markers from stderr.
fn check_download(captured: &Captured) -> Result<PathBuf> {
    if !captured.status.success() {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "yt-dlp başarısız oldu ({0}): {1}",
                "yt-dlp failed ({0}): {1}",
                describe(&captured.status),
                bounded_tail(captured.stderr.trim(), 400)
            )
        );
    }
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let post_processing = captured
        .stderr
        .lines()
        .filter(|line| line.starts_with(POSTPROCESS_PREFIX));
    let lines = stdout
        .lines()
        .map(|line| (line, false))
        .chain(post_processing.map(|line| (line, true)));
    let (mut progressed, mut processed) = (false, false);
    let mut completed = HashSet::new();
    let mut file = None;
    let mut gauge = DownloadGauge::new(0);
    for (line, from_post_processor) in lines {
        handle_machine_line(
            line,
            Instant::now(),
            &mut |update| {
                if from_post_processor {
                    processed |= update.processing;
                } else {
                    progressed |= !update.processing && update.downloaded > 0;
                }
            },
            &mut completed,
            &mut file,
            &mut gauge,
        );
    }
    if !progressed {
        bail!(
            "{}",
            crate::i18n::ui(
                "yt-dlp hiç ilerleme satırı yazdırmadı",
                "yt-dlp printed no progress line"
            )
        );
    }
    if !processed {
        bail!(
            "{}",
            crate::i18n::ui(
                "yt-dlp hiç son işlem satırı yazdırmadı",
                "yt-dlp printed no post-processor line"
            )
        );
    }
    let file = file.context(crate::i18n::ui(
        "yt-dlp çıktı dosyası satırı yazdırmadı",
        "yt-dlp printed no output file line",
    ))?;
    if !completed.contains(&file) {
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "çıktı dosyası {0} yok",
                "the output file {0} does not exist",
                file.display()
            )
        );
    }
    Ok(file)
}

/// Windows always reports an exit code.
fn describe(status: &ExitStatus) -> String {
    crate::i18n::ui_format!(
        "çıkış kodu {0}",
        "exit code {0}",
        status.code().unwrap_or_default()
    )
}

/// Stub tools that behave like real ones for a good set and misbehave in one chosen way.
#[cfg(test)]
pub(crate) mod stubs {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Fault {
        None,
        ClipFailsToRun,
        ClipWritesNothing,
        DownloadExitsWithError,
        NoProgressLine,
        NoPostprocessLine,
        NoFileLine,
        ProgressOnlyOnStderr,
        FileLineNamesAMissingFile,
        ProbeExitsWithError,
        ProbePrintsNoJson,
        ProbeFindsNoAudio,
        ProbeFindsNoVideo,
        StreamsDisagreeInLength,
    }

    impl Fault {
        pub(crate) const FAILING: [Fault; 13] = [
            Fault::ClipFailsToRun,
            Fault::ClipWritesNothing,
            Fault::DownloadExitsWithError,
            Fault::NoProgressLine,
            Fault::NoPostprocessLine,
            Fault::NoFileLine,
            Fault::ProgressOnlyOnStderr,
            Fault::FileLineNamesAMissingFile,
            Fault::ProbeExitsWithError,
            Fault::ProbePrintsNoJson,
            Fault::ProbeFindsNoAudio,
            Fault::ProbeFindsNoVideo,
            Fault::StreamsDisagreeInLength,
        ];
    }

    const PROGRESS: &str = "SSDOWNLOAD_PROGRESS\tdownloading\t1024\t12072\n\
        SSDOWNLOAD_PROGRESS\tdownloading\t12072\t12072\n\
        SSDOWNLOAD_PROGRESS\tfinished\t12072\t12072\n";
    const POSTPROCESS: &str = "SSDOWNLOAD_POSTPROCESS\tstarted\nSSDOWNLOAD_POSTPROCESS\tfinished\n";

    fn captured(code: u32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            status: ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.to_owned(),
        }
    }

    /// ffprobe's answer for the merged file: streams of about one second, as the real tool
    /// prints them for a Matroska file.
    pub(crate) fn probe_output(video: bool, audio: bool, audio_length: &str) -> String {
        let mut streams = Vec::new();
        if video {
            streams.push(serde_json::json!({
                "index": 0, "codec_name": "mpeg4", "codec_type": "video", "width": 160,
                "height": 90, "r_frame_rate": "10/1", "avg_frame_rate": "10/1",
                "start_time": "0.000000", "disposition": {"default": 1},
                "tags": {"DURATION": "00:00:01.000000000"},
            }));
        }
        if audio {
            streams.push(serde_json::json!({
                "index": 1, "codec_name": "aac", "codec_type": "audio", "sample_rate": "44100",
                "channels": 2, "start_time": "0.000000", "disposition": {"default": 1},
                "tags": {"DURATION": audio_length},
            }));
        }
        serde_json::json!({
            "streams": streams,
            "format": {"format_name": "matroska,webm", "start_time": "0.000000", "duration": "1.023000"},
        })
        .to_string()
    }

    fn run_with(fault: Fault) -> impl FnMut(&Path, &[String], Duration) -> Result<Captured> {
        move |executable: &Path, args: &[String], _timeout: Duration| {
            let name = executable
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            Ok(match name.as_str() {
                "ffmpeg.exe" => {
                    if fault != Fault::ClipWritesNothing {
                        for arg in args
                            .iter()
                            .filter(|arg| arg.ends_with(".mp4") || arg.ends_with(".m4a"))
                        {
                            fs::write(arg, b"media").unwrap();
                        }
                    }
                    if fault == Fault::ClipFailsToRun {
                        captured(1, "", "Unknown encoder 'mpeg4'")
                    } else {
                        captured(0, "", "")
                    }
                }
                "yt-dlp.exe" => {
                    let template =
                        &args[args.iter().position(|arg| arg == "--output").unwrap() + 1];
                    let output = PathBuf::from(
                        template
                            .replace("%(id)s", "selftest")
                            .replace("%(ext)s", "mkv"),
                    );
                    fs::create_dir_all(output.parent().unwrap()).unwrap();
                    let reported = if fault == Fault::FileLineNamesAMissingFile {
                        output.with_file_name("other.mkv")
                    } else {
                        fs::write(&output, b"media").unwrap();
                        output
                    };
                    let file_line = if fault == Fault::NoFileLine {
                        String::new()
                    } else {
                        format!(
                            "SSDOWNLOAD_FILE\t{}\n",
                            serde_json::to_string(&reported.to_string_lossy()).unwrap()
                        )
                    };
                    let progress =
                        if matches!(fault, Fault::NoProgressLine | Fault::ProgressOnlyOnStderr) {
                            ""
                        } else {
                            PROGRESS
                        };
                    let mut stderr =
                        String::from("Reading options from STDIN - EOF (Ctrl+Z) to end:\n");
                    if fault == Fault::ProgressOnlyOnStderr {
                        stderr.push_str(PROGRESS);
                    }
                    if fault != Fault::NoPostprocessLine {
                        stderr.push_str(POSTPROCESS);
                    }
                    if fault == Fault::DownloadExitsWithError {
                        stderr.push_str("ERROR: unable to download video data\n");
                        return Ok(captured(1, &format!("{progress}{file_line}"), &stderr));
                    }
                    captured(0, &format!("{progress}{file_line}"), &stderr)
                }
                "ffprobe.exe" => match fault {
                    Fault::ProbeExitsWithError => captured(
                        1,
                        &probe_output(true, true, "00:00:01.023000000"),
                        "Invalid data found",
                    ),
                    Fault::ProbePrintsNoJson => captured(0, "not json", ""),
                    Fault::ProbeFindsNoAudio => captured(0, &probe_output(true, false, ""), ""),
                    Fault::ProbeFindsNoVideo => {
                        captured(0, &probe_output(false, true, "00:00:01.023000000"), "")
                    }
                    Fault::StreamsDisagreeInLength => {
                        captured(0, &probe_output(true, true, "00:00:02.500000000"), "")
                    }
                    _ => captured(0, &probe_output(true, true, "00:00:01.023000000"), ""),
                },
                other => panic!("unexpected tool {other}"),
            })
        }
    }

    pub(crate) fn fake_tools() -> tools::VerifiedTools {
        let root = PathBuf::from("C:\\fake");
        tools::VerifiedTools::staged(
            root.join("yt-dlp.exe"),
            root.join("deno.exe"),
            root.join("ffmpeg.exe"),
            root.join("ffprobe.exe"),
        )
    }

    /// The self-test of a set whose tools misbehave as `fault` says.
    pub(crate) fn outcome(fault: Fault) -> Result<()> {
        let scratch =
            std::env::temp_dir().join(format!("ssdownload-selftest-{}", uuid::Uuid::new_v4()));
        let result = self_test_with(&fake_tools(), &scratch, &mut run_with(fault));
        let _ = fs::remove_dir_all(scratch);
        result
    }

    /// Runs the self-test of a good set and returns the tools it started with their arguments.
    pub(crate) fn recorded_run() -> Vec<(PathBuf, Vec<String>)> {
        let scratch =
            std::env::temp_dir().join(format!("ssdownload-selftest-{}", uuid::Uuid::new_v4()));
        let mut seen = Vec::new();
        let mut inner = run_with(Fault::None);
        self_test_with(&fake_tools(), &scratch, &mut |executable, args, timeout| {
            seen.push((executable.to_path_buf(), args.to_vec()));
            inner(executable, args, timeout)
        })
        .unwrap();
        let _ = fs::remove_dir_all(scratch);
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::stubs::*;
    use super::*;

    #[test]
    fn a_set_that_behaves_passes() {
        outcome(Fault::None).unwrap();
    }

    /// What each way of failing is reported with, in English.
    fn english_reasons() -> Vec<(Fault, String)> {
        crate::i18n::with_language("en", || {
            // What the application's own probe validation says about the stub's output.
            let finding = |video, audio, length| {
                let probe = serde_json::from_str(&probe_output(video, audio, length)).unwrap();
                let error = validate_probe(&probe, &probe_request(), &MediaInfo::default())
                    .expect_err("the stub output is invalid");
                format!("{error:#}")
            };
            let table = vec![
                (
                    Fault::ClipFailsToRun,
                    "did not generate the test clip".into(),
                ),
                (
                    Fault::ClipWritesNothing,
                    "did not generate the test clip".into(),
                ),
                (
                    Fault::DownloadExitsWithError,
                    "yt-dlp failed (exit code 1)".into(),
                ),
                (Fault::NoProgressLine, "no progress line".into()),
                (Fault::ProgressOnlyOnStderr, "no progress line".into()),
                (Fault::NoPostprocessLine, "no post-processor line".into()),
                (Fault::NoFileLine, "no output file line".into()),
                (Fault::FileLineNamesAMissingFile, "does not exist".into()),
                (
                    Fault::ProbeExitsWithError,
                    "ffprobe could not read the output".into(),
                ),
                (
                    Fault::ProbePrintsNoJson,
                    "ffprobe did not print JSON".into(),
                ),
                (Fault::ProbeFindsNoAudio, finding(true, false, "")),
                (
                    Fault::ProbeFindsNoVideo,
                    finding(false, true, "00:00:01.023000000"),
                ),
                (
                    Fault::StreamsDisagreeInLength,
                    finding(true, true, "00:00:02.500000000"),
                ),
            ];
            for fault in Fault::FAILING {
                assert!(
                    table.iter().any(|(listed, _)| *listed == fault),
                    "{fault:?} has no expectation"
                );
            }
            table
        })
    }

    fn failure_text(fault: Fault) -> String {
        let error = outcome(fault).expect_err(&format!("{fault:?} passed the self-test"));
        format!("{error:#}")
    }

    #[test]
    fn every_way_a_set_can_fail_is_reported() {
        let table = english_reasons();
        crate::i18n::with_language("en", || {
            let prefix = crate::i18n::ui_format!(
                "Medya özdenetimi başarısız: {0}",
                "The media self-test failed: {0}",
                ""
            );
            for (fault, reason) in &table {
                let text = failure_text(*fault);
                assert!(text.starts_with(&prefix), "{fault:?}: {text}");
                assert!(text.contains(reason.as_str()), "{fault:?}: {text}");
            }
        });
    }

    #[test]
    fn every_failure_is_reported_in_the_language_of_the_interface() {
        let table = english_reasons();
        for code in ["tr", "ar", "de", "es", "fr", "pt"] {
            crate::i18n::with_language(code, || {
                for (fault, reason) in &table {
                    let text = failure_text(*fault);
                    assert!(!text.contains(reason.as_str()), "{code} {fault:?}: {text}");
                    assert!(!text.contains("exit code"), "{code} {fault:?}: {text}");
                }
            });
        }
    }

    #[test]
    fn the_download_runs_with_the_applications_arguments_and_markers() {
        let seen = recorded_run();

        let names = seen
            .iter()
            .map(|(executable, _)| {
                executable
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["ffmpeg.exe", "yt-dlp.exe", "ffprobe.exe"]);
        let download = &seen[1].1;
        let common = common_args(
            Path::new("C:\\fake\\deno.exe"),
            Path::new("C:\\fake\\ffmpeg.exe"),
            false,
        );
        assert!(download.starts_with(&common), "{download:?}");
        for argument in marker_args()
            .iter()
            .chain(&["--enable-file-urls".to_owned()])
        {
            assert!(
                download.contains(argument),
                "{argument} is missing from {download:?}"
            );
        }
    }
}
