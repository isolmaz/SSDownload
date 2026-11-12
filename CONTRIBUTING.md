# Contributing

Bug reports and pull requests are welcome.

## Issues

- Use the **Bug report** form. It asks for the SSDownload version (**Help → About**), your Windows version and the `SSD-…` error code if one was shown.
- Never attach cookies, signed download addresses or private files. Addresses in a **Report problem** or **Diagnostics** ZIP are redacted, but the page markup (`sayfa.html`) is not, so look through it before sharing. It is stored as escaped text that opens without running anything.
- Security problems go privately through **Security → Report a vulnerability**, not into an issue (see [SECURITY.md](SECURITY.md)).
- Only report problems with content you have the right to download. DRM-protected streams are out of scope.

## Build

Requirements: Windows 10/11 x64, Visual Studio C++ Build Tools with the Windows SDK, [rustup](https://rustup.rs) (the toolchain pinned in `rust-toolchain.toml` installs itself), Python 3 for the translation catalogs and Node.js 20+ for the extension tests.

```powershell
cargo build --locked
cargo run --locked --bin ssdownload -- --data-dir .qa\dev-profile   # separate profile, keeps your real queue untouched
```

`ssdownload-cli.exe` must stay next to `ssdownload.exe`. To try the extension, load `browser\chromium` unpacked and register your build with `target\debug\ssdownload.exe --register-host` (register your installed copy again afterwards).

## Checks

Pull requests and pushes to `main` run these checks on GitHub Actions (`.github/workflows/ci.yml`) on Windows: `cargo fmt --check`, Clippy with warnings as errors, `cargo test`, the translation catalog check, the extension tests, the Firefox add-on check and `cargo build`. The cargo commands run with `--locked`. Run the checks on your machine first:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check.ps1
```

The script runs `cargo fmt --check`, Clippy with warnings as errors, the Rust tests, the translation catalog check (`python scripts/i18n.py check`), the extension tests (`node --test`) and `cargo audit` when it is installed. A step whose tool is missing is reported as skipped. `scripts\install-hooks.ps1` installs the script as a pre-push hook. CI does not run `cargo audit`, e2e checks, packaging or signing; releases stay a local maintainer task.

The Firefox add-on has its own two checks, which CI runs in its **Firefox extension** step:

```powershell
python scripts\sync-firefox-extension.py --check
npx --yes web-ext@10.8.0 lint --source-dir browser/firefox --output text
```

The first fails when `browser\firefox` differs from the copy the script generates from `browser\chromium`, including a file that exists only in `browser\firefox`; the second runs the Mozilla linter over the add-on.

Two kinds of test run only on request.

The HLS end-to-end tests download from a local server through the media tools the app installed in `%LOCALAPPDATA%\SSDownload\tools`. They cover the app's own HLS downloader and the yt-dlp path; the yt-dlp one waits out its fragment retries and takes about five minutes. The same command runs the self-test that gates a tool update (`the_installed_tools_pass_the_self_test`) and checks that a tool set can be deleted right after its self-test, passed or failed (`a_tool_set_can_be_deleted_right_after_its_self_test`):

```powershell
cargo test --lib hls_tests -- --ignored
```

The download benchmarks (`bench_*` in `src/transfer/loopback.rs`) fetch a 128 MiB file from the throttled loopback test server in `src/test_http.rs`, check its SHA-256 each time and print seconds and MiB/s per scenario. They take several minutes; run them one at a time:

```powershell
cargo test --lib bench_ -- --ignored --nocapture --test-threads=1
```

## Code layout

```text
src/main.rs          Mode selection: desktop, CLI forwarding or native messaging host
src/app.rs           Every front (window, CLI, extension) sends a typed Action here
src/engine/          The single engine thread that owns jobs, settings and the SQLite store
src/transfer.rs      Segmented HTTP/HTTPS/FTP downloads (libcurl)
src/media/           yt-dlp / FFmpeg pipeline; src/tools.rs installs and verifies the tools
src/gui/native/      The Win32 interface
src/i18n.rs          Interface languages; src/i18n_catalog.rs is generated from i18n/<code>.json
src/bridge.rs        Native messaging and the local named pipe
src/update.rs        Signed update feed
src/test_http.rs     Configurable loopback HTTP server for tests (test builds only)
browser/chromium/    The Chrome/Edge extension (Manifest V3), loadable as is
browser/firefox/     The Firefox add-on, generated from browser/chromium by scripts/sync-firefox-extension.py (except its manifest.json)
browser/tests/       Extension tests (node:test, no dependencies)
packaging/           NSIS installer and browser registration scripts
scripts/             Local checks, error-code generator, Firefox sync and release scripts
```

## Conventions

- Behaviour is general and never tied to one site: no code checks a particular site's host name, address or page layout. A fix follows from what the source does (how the server answers, what the playlist or the player contains), so it applies to every site that behaves the same way.
- Match the surrounding code. Identifiers, comments and documentation are in English.
- Interface text goes through `crate::i18n::ui("Türkçe", "English")`, or `crate::i18n::ui_format!("Türkçe {0}", "English {0}", value)` for a template, with both languages written at the call. The compiler checks that a translated template uses every argument of the call. A count has one text for one item and one for several (`1 download added`, `{count} downloads added`), never an ending in parentheses such as `download(s)`.
- Arabic, Spanish, Portuguese, French and German translate the English text in `i18n/<code>.json` (English text → translation). After adding or changing a text, add its translation to each catalog and run `python scripts/i18n.py build`, which regenerates `src/i18n_catalog.rs`.
- `python scripts/i18n.py check` (also run by `scripts/check.ps1` and CI) lists missing and stale entries, placeholders that differ from the English text, `&` access letters and tab shortcuts that differ, two items of one menu that share an access letter in any language, a label or message of a window written as a bare literal instead of through `ui`, an em dash (U+2014) in any string, an ending in parentheses such as `file(s)`, and a media tool name in a text the user reads. The extension's tests enforce the em dash and tool name rules for its texts.
- The browser extension's text lives in `browser/chromium/i18n.js` (`t("key")`, every language with the same keys and `{0}` placeholders). The extension follows the app's language, and `node --test browser/tests/*.test.js` checks all languages. The installer's texts are the `LangString` lines of `packaging/installer.nsi`.
- A text the user reads names no program behind a media component (yt-dlp, FFmpeg, ffprobe, Deno, BtbN). It says what the component does (`crate::tools::component_label`). A text that carries what such a program printed goes through `crate::tools::without_component_names` where the user reads it, developer mode included. Logs, the job log, diagnostics and reports keep the names.
  - The filter replaces a name wherever it stands, also in an address a program printed and in a path below the tools folder of the application.
  - The user's own strings stay whole, even when they hold a whole program name: a file path (a word with a `\` or one that begins like `C:/`, `/` or `./`) and every string the caller passes as the user's own (the address, name, path and folder of the job, what is typed in the window, the folders of the profile, of the downloads and of the folder rules). A string that is itself a program name keeps nothing, and a program below the tools folder is hidden whatever the caller passes.
  - A template that names a component either starts with its label (`component_title`) or follows a colon or stands in parentheses (`component_label`), in every language, so no language needs an article or a case for it.
- Threads are named `std::thread`s (no async runtime). Only the engine thread changes jobs, settings and the store; everything else sends it a command or reads a snapshot.
- Start helper programs with argument arrays, never through a shell. Validate input at trust boundaries with the helpers in `src/validation.rs`.
- User-visible failures use `bail_code!` with a permanent `SSD-<area>-<number>` code. Codes are never renumbered or reused: edit the table in `scripts/generate-error-codes.py` and run it; it rewrites `src/error_codes.rs` and `browser/chromium/codes.js`.
- Tests sit next to the code in `#[cfg(test)] mod tests` and use local fixtures (`127.0.0.1`, `.example` hosts) only, never live sites. They create files only in temporary directories: in a test build `AppPaths::new` puts the download folder inside the profile directory and the cookie jars in a scratch folder of the process, so no test fills your Downloads folder or the profile of an installed copy. The window tests of `src/gui/native` run on private Windows desktops, so none of their windows or focus changes reaches your screen. A test takes a free desktop for its own use through `on_private_desktop` and hands it on afterwards: the test process keeps a few desktops for its whole run instead of making one per window, because Windows keeps a little kernel memory for every desktop that was created, also after it is closed. A thread that creates windows must end before the test body returns, so the next test finds the desktop empty.
- Update the affected documentation ([README.md](README.md), [docs/GUIDE.md](docs/GUIDE.md), [PRIVACY.md](PRIVACY.md), [SECURITY.md](SECURITY.md)) in the same change.

Contributions are licensed under the [MIT License](LICENSE.txt).

## Releases (maintainer)

The application ships from this repository's GitHub Releases: the Setup and portable ZIP and the signed update feed (`version.json`, `version.json.sig`). The Chrome and Edge extension ships through the Chrome Web Store; the Firefox add-on is packaged separately and needs a signature from Mozilla for a permanent install. Requirements: NSIS 3.10 (`%LOCALAPPDATA%\Programs\nsis-3.10`), OpenSSL, an authenticated `gh`, and the two private keys outside the repository: `%LOCALAPPDATA%\SSDownload\keys\update-signing.key` (Ed25519, must match `FEED_PUBLIC_KEY` in `src/update.rs`) and `extension.key` (fixes the extension ID). If either key is lost, installed copies cannot verify updates or the extension ID changes, so keep an offline backup.

1. Set the new version in `Cargo.toml`, `browser/chromium/manifest.json` and `browser/firefox/manifest.json`, run `cargo check` to update `Cargo.lock`, and run `scripts\check.ps1`. Compare the bridge versions with the [compatibility table](#app-and-extension-compatibility): a change blocks the release until the conditions there hold.
2. `scripts\package.ps1` builds the release and writes the Setup, portable ZIP, symbols ZIP and `SHA256SUMS` to `artifacts\<version>\app`.
3. `scripts\package-extension.ps1` writes the store package `artifacts\<version>\extension\SSDownload-<version>-store.zip` (the manifest without `key`). Upload it to the Chrome Web Store item `hgndggnlfpnflkmnbddmcnfniamckham`. The first upload of the item must be built with `-FirstUpload`: it adds `key.pem`, which makes the store keep the manifest-key ID that the app registers and the native host allows. Never publish that ZIP.
4. `scripts\package-firefox.ps1` writes an unsigned XPI of `browser\firefox` to `artifacts\<version>\extension`. Firefox installs it permanently only when Mozilla has signed it for an addons.mozilla.org listing, so signing and uploading the XPI is a manual maintainer step of its own, separate from the Chrome Web Store ZIP. No script publishes or uploads it.
5. Add the release to the compatibility table. Commit and push, then `scripts\prepare-release.ps1 -PackageDir artifacts\<version>\app -Notes "<what changed>" -Publish` signs the feed, checks every checksum and creates the GitHub release with all assets. It refuses a `-Repository` that differs from `DEFAULT_FEED_BASE` in `src/update.rs`, and after publishing it reads `version.json` anonymously and fails unless the public feed serves the new version.

Optional Authenticode signing: `scripts\package.ps1 -SigningProfile <metadata.json>` (Azure Artifact Signing) or `-CertificateThumbprint <sha1> -TimestampServer <url>`.

### App and extension compatibility

The app and the extension work together only while they speak the same bridge protocol and capability version; their own version numbers play no part. The values live in `PROTOCOL_VERSION` (`src/bridge.rs`) and `CAPABILITY_VERSION` (`src/model.rs`). The extension sends the protocol in every request (`browser/chromium/background.js`, `relay-background.js`) and checks both in `validateCapabilities`. While neither value changes, any app works with any extension, so the store item may lag the app.

The two sides never update together. Chrome installs an approved store version on its own within hours, an installed app updates only when its user accepts the update, and a store review can take days. Every pairing of an old and a new side therefore exists for a while. A release that changes either value is blocked until:

- the new app still answers the previous protocol and capability version, and the new extension still works with an app that reports the previous values, each covered by a test;
- the extension of that release is submitted to the Chrome Web Store together with the app release.

A capability that only adds a field to the capabilities reply (`browser_takeover`, `add_status`) changes neither value: the extension reads an absent field as off, so an app without it is unaffected. The same holds for an optional field of a request (`top_page_url` of a media handoff): an app without it ignores the field, because the request of a media handoff does not reject unknown fields, and an app reads an extension's absent one as not sent. What the field enables needs the new app and the new extension together; with either side lacking it, the media tool's own error for a frame stays.

Dropping a previous value is a later release of its own, published only after the store serves an extension that speaks the new values.

| Releases | Bridge protocol | Capability version | Also accepts |
|---|---|---|---|
| 1.6.1 – 1.11.0 | 2 | 3 | – |
