<p align="center">
  <img src="assets/app-icon.svg" width="88" alt="SSDownload icon">
</p>

<h1 align="center">SSDownload</h1>

<p align="center">
  A fast, private download manager for Windows: files, video and audio, with a browser button that sends media straight to the app.
</p>

<p align="center">
  <a href="https://github.com/isolmaz/SSDownload/releases/latest"><img src="https://img.shields.io/github/v/release/isolmaz/SSDownload?label=download" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/Windows-10%20%7C%2011%20x64-0078d4" alt="Windows 10 and 11, 64-bit">
  <a href="https://github.com/isolmaz/SSDownload/actions/workflows/ci.yml"><img src="https://github.com/isolmaz/SSDownload/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI status"></a>
  <a href="LICENSE.txt"><img src="https://img.shields.io/badge/license-MIT-green" alt="MIT license"></a>
</p>

<p align="center">
  <img src="docs/media/download.gif" width="860" alt="Three links pasted into New download, downloaded over eight connections each, with completion cards">
</p>

## Features

- **Fast, resumable downloads** over up to 16 connections per file. A download resumes only when the server proves the file is unchanged, and a finished file never overwrites an existing one.
- **Video and audio** from the page you are watching: choose the quality, container, audio and subtitle tracks, or save as MP3. Every result is checked with FFmpeg before it is published. Live streams are recorded, from their beginning where the site keeps it, and a live video recording can be finished early with what was recorded so far.
- **Browser extension** for Chrome and Edge: a download button on videos, right-click entries and optional download takeover.
- **A tidy queue:** drag to reorder, pause and resume, speed limits, a mini panel, completion cards and tray controls.
- **For larger jobs:** named queues with time windows, folder rules, batch ranges, a site crawler, a command line and signed automatic updates.
- **Private by design:** no account, no telemetry, no server. Everything stays on your PC.

<p align="center">
  <img src="docs/media/video.gif" width="860" alt="The SSDownload button on a video opens the quality picker; 720p is chosen and downloaded">
</p>

## Install

1. Download **`SSDownload-<version>-Setup.exe`** (or the portable ZIP) from the [latest release](https://github.com/isolmaz/SSDownload/releases/latest) and run it. No administrator rights are needed.
2. The installer adds the browser extension to Chrome and Edge. The next time you open the browser, click **Enable extension** on its "New extension added" notice. No notice? Add it from the [Chrome Web Store](https://chromewebstore.google.com/detail/hgndggnlfpnflkmnbddmcnfniamckham), or use **Tools → Browser setup** to load the bundled copy without the store.
3. That's it. The installer downloads media support (yt-dlp, FFmpeg, Deno), each file verified against its published SHA-256. Without a connection, the app installs it on its first start instead.

The builds are not code-signed yet, so SmartScreen may warn on first run. The release page lists the SHA-256 of every file. Installed copies update themselves after you accept the signed update.

The app speaks **Turkish and English**: it starts in your Windows display language, and **Settings → General** switches it. The browser extension and the command line follow the app's language.

## Documentation

| | |
|---|---|
| [User guide](docs/GUIDE.md) | Menus, settings, shortcuts, recovery and the command line |
| [Privacy](PRIVACY.md) | What is stored, and the only connections the app makes |
| [Security](SECURITY.md) | Security model and how to report a vulnerability privately |
| [Contributing](CONTRIBUTING.md) | Building from source, local checks and code layout |

## Build from source

Requires Windows 10/11 x64, the Visual Studio C++ Build Tools and [rustup](https://rustup.rs).

```powershell
cargo build --release --locked
```

Checks run locally with `scripts\check.ps1` and on every pull request in GitHub Actions. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## License

[MIT](LICENSE.txt). yt-dlp, FFmpeg and Deno are not bundled; the app downloads them from their official releases. See [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

Download only content you have the right to download. DRM-protected streams are not supported.
