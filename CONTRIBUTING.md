# Contributing

Bug reports and pull requests are welcome.

## Issues

- Include the SSDownload version (**Help → About**), your Windows version and the `SSD-…` error code if one was shown.
- Never attach cookies, signed download addresses or private files. Addresses in a **Report problem** or **Diagnostics** ZIP are redacted, but the page markup (`sayfa.html`) is not; look through it before sharing.
- Security problems go privately through **Security → Report a vulnerability**, not into an issue (see [SECURITY.md](SECURITY.md)).
- Only report problems with content you have the right to download. DRM-protected streams are out of scope.

## Build

Requirements: Windows 10/11 x64, Visual Studio C++ Build Tools with the Windows SDK, [rustup](https://rustup.rs) (the toolchain pinned in `rust-toolchain.toml` installs itself) and Node.js 20+ for the extension tests.

```powershell
cargo build --locked
cargo run --locked --bin ssdownload -- --data-dir .qa\dev-profile   # separate profile, keeps your real queue untouched
```

`ssdownload-cli.exe` must stay next to `ssdownload.exe`. To try the extension, load `browser\chromium` unpacked and register your build with `target\debug\ssdownload.exe --register-host` (register your installed copy again afterwards).

## Checks

Pull requests and pushes to `main` run the same checks on GitHub Actions (`.github/workflows/ci.yml`: `cargo fmt --check`, Clippy with warnings as errors, `cargo test`, the extension tests and `cargo build`, all `--locked` on Windows). Run them on your machine first:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check.ps1
```

It runs `cargo fmt --check`, Clippy with warnings as errors, the Rust tests, the extension tests (`node --test`) and `cargo audit` when it is installed. `scripts\install-hooks.ps1` installs it as a pre-push hook. CI does not run `cargo audit`, e2e checks, packaging or signing; releases stay a local maintainer task.

The HLS end-to-end tests download from a local server through the media tools the app installed in `%LOCALAPPDATA%\SSDownload\tools`, so they run only on request. They cover the app's own HLS downloader and the yt-dlp path; the yt-dlp one waits out its fragment retries and takes about five minutes:

```powershell
cargo test --lib hls_tests -- --ignored
```

## Code layout

```text
src/main.rs          Mode selection: desktop, CLI forwarding or native messaging host
src/app.rs           Every front (window, CLI, extension) sends a typed Action here
src/engine/          The single engine thread that owns jobs, settings and the SQLite store
src/transfer.rs      Segmented HTTP/HTTPS/FTP downloads (libcurl)
src/media/           yt-dlp / FFmpeg pipeline; src/tools.rs installs and verifies the tools
src/gui/native/      The Win32 interface
src/bridge.rs        Native messaging and the local named pipe
src/update.rs        Signed update feed
browser/chromium/    The Chrome/Edge extension (Manifest V3), loadable as is
browser/tests/       Extension tests (node:test, no dependencies)
packaging/           NSIS installer and browser registration scripts
scripts/             Local checks, error-code generator and release scripts
```

## Conventions

- Behaviour is general and never tied to one site: no code checks a particular site's host name, address or page layout. A fix follows from what the source does (how the server answers, what the playlist or the player contains), so it applies to every site that behaves the same way.
- Match the surrounding code. Identifiers, comments and documentation are in English; interface text goes through `crate::i18n::ui("Türkçe", "English")`, or `crate::i18n::ui_format!("Türkçe {0}", "English {0}", value)` for a template, with both languages written at the call. Arabic, Spanish, Portuguese, French and German translate the English text in `i18n/<code>.json` (English text → translation). After adding or changing a text, add its translation to each catalog and run `python scripts/i18n.py build`, which regenerates `src/i18n_catalog.rs`; `python scripts/i18n.py check` (also run by `scripts/check.ps1` and CI) lists missing and stale entries, changed placeholders, `&` access letters and tab shortcuts. A translated template is checked by the compiler as well: it has to use every argument of the call. The browser extension's text lives in `browser/chromium/i18n.js` (`t("key")`, every language with the same keys and `{0}` placeholders; the extension follows the app's language and `node --test browser/tests/*.test.js` checks all languages). The installer's texts are the `LangString` lines of `packaging/installer.nsi`.
- Threads are named `std::thread`s (no async runtime). Only the engine thread changes jobs, settings and the store; everything else sends it a command or reads a snapshot.
- Start helper programs with argument arrays, never through a shell. Validate input at trust boundaries with the helpers in `src/validation.rs`.
- User-visible failures use `bail_code!` with a permanent `SSD-<area>-<number>` code. Codes are never renumbered or reused: edit the table in `scripts/generate-error-codes.py` and run it; it rewrites `src/error_codes.rs` and `browser/chromium/codes.js`.
- Tests sit next to the code in `#[cfg(test)] mod tests` and use local fixtures (`127.0.0.1`, `.example` hosts) only, never live sites.
- Update the affected documentation ([README.md](README.md), [docs/GUIDE.md](docs/GUIDE.md), [PRIVACY.md](PRIVACY.md), [SECURITY.md](SECURITY.md)) in the same change.

Contributions are licensed under the [MIT License](LICENSE.txt).

## Releases (maintainer)

The application ships from this repository's GitHub Releases: the Setup and portable ZIP and the signed update feed (`version.json`, `version.json.sig`). The browser extension ships through the Chrome Web Store. Requirements: NSIS 3.10 (`%LOCALAPPDATA%\Programs\nsis-3.10`), OpenSSL, an authenticated `gh`, and the two private keys outside the repository: `%LOCALAPPDATA%\SSDownload\keys\update-signing.key` (Ed25519, must match `FEED_PUBLIC_KEY` in `src/update.rs`) and `extension.key` (fixes the extension ID). Losing either key means installed copies can no longer verify updates or the extension ID changes, so keep an offline backup.

1. Set the new version in `Cargo.toml` and `browser/chromium/manifest.json`, run `cargo check` to update `Cargo.lock`, and run `scripts\check.ps1`.
2. `scripts\package.ps1` builds the release and writes the Setup, portable ZIP, symbols ZIP and `SHA256SUMS` to `artifacts\<version>\app`.
3. `scripts\package-extension.ps1` writes the store package `artifacts\<version>\extension\SSDownload-<version>-store.zip` (the manifest without `key`). Upload it to the Chrome Web Store item `hgndggnlfpnflkmnbddmcnfniamckham`. The first upload of the item must be built with `-FirstUpload`: it adds `key.pem`, which makes the store keep the manifest-key ID that the app registers and the native host allows. Never publish that ZIP.
4. Commit and push, then `scripts\prepare-release.ps1 -PackageDir artifacts\<version>\app -Notes "<what changed>" -Publish` signs the feed, checks every checksum and creates the GitHub release with all assets. It refuses a `-Repository` that differs from `DEFAULT_FEED_BASE` in `src/update.rs`, and after publishing it reads `version.json` anonymously and fails unless the public feed serves the new version.

Optional Authenticode signing: `scripts\package.ps1 -SigningProfile <metadata.json>` (Azure Artifact Signing) or `-CertificateThumbprint <sha1> -TimestampServer <url>`.
