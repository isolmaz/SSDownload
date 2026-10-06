# Contributing

Bug reports and pull requests are welcome.

## Issues

- Include the SSDownload version (**Help → About**), your Windows version and the `SSD-…` error code if one was shown.
- Never attach cookies, signed download addresses or private files. A **Report failure** or **Diagnostics** ZIP is redacted, but look through it before sharing.
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

There is no hosted CI and GitHub Actions is disabled for this repository. Every check runs on your machine:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check.ps1
```

It runs `cargo fmt --check`, Clippy with warnings as errors, the Rust tests, the extension tests (`node --test`) and `cargo audit` when it is installed. `scripts\install-hooks.ps1` installs it as a pre-push hook. Please do not add workflow files.

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

- Match the surrounding code. Identifiers, comments and documentation are in English; interface text goes through `crate::i18n::ui("Türkçe", "English")` with both languages.
- Threads are named `std::thread`s (no async runtime). Only the engine thread changes jobs, settings and the store; everything else sends it a command or reads a snapshot.
- Start helper programs with argument arrays, never through a shell. Validate input at trust boundaries with the helpers in `src/validation.rs`.
- User-visible failures use `bail_code!` with a permanent `SSD-<area>-<number>` code. Codes are never renumbered or reused: edit the table in `scripts/generate-error-codes.py` and run it; it rewrites `src/error_codes.rs` and `browser/chromium/codes.js`.
- Tests sit next to the code in `#[cfg(test)] mod tests` and use local fixtures (`127.0.0.1`, `.example` hosts) only, never live sites.
- Update the affected documentation ([README.md](README.md), [docs/GUIDE.md](docs/GUIDE.md), [PRIVACY.md](PRIVACY.md), [SECURITY.md](SECURITY.md)) in the same change.

Contributions are licensed under the [MIT License](LICENSE.txt).

## Releases (maintainer)

Everything ships from this repository's GitHub Releases: the Setup and portable ZIP, the signed update feed (`version.json`, `version.json.sig`) and the extension's `update.xml` and CRX. Requirements: NSIS 3.10 (`%LOCALAPPDATA%\Programs\nsis-3.10`), OpenSSL, Google Chrome (packs the CRX), an authenticated `gh`, and the two private keys outside the repository: `%LOCALAPPDATA%\SSDownload\keys\update-signing.key` (Ed25519, must match `FEED_PUBLIC_KEY` in `src/update.rs`) and `extension.key` (fixes the extension ID). Losing either key means installed copies can no longer verify updates or the extension ID changes, so keep an offline backup.

1. Set the new version in `Cargo.toml` and `browser/chromium/manifest.json`, run `cargo check` to update `Cargo.lock`, and run `scripts\check.ps1`.
2. `scripts\package.ps1` builds the release and writes the Setup, portable ZIP, symbols ZIP and `SHA256SUMS` to `artifacts\<version>\app`.
3. `scripts\package-extension.ps1` writes the CRX and `update.xml` to `artifacts\<version>\extension`.
4. Commit and push, then `scripts\prepare-release.ps1 -PackageDir artifacts\<version>\app -ExtensionDir artifacts\<version>\extension -Notes "<what changed>" -Publish` signs the feed, checks every checksum and creates the GitHub release with all assets.

Optional Authenticode signing: `scripts\package.ps1 -SigningProfile <metadata.json>` (Azure Artifact Signing) or `-CertificateThumbprint <sha1> -TimestampServer <url>`.
