# Security

## Reporting a vulnerability

Please report security problems **privately** through GitHub: **Security → Report a vulnerability** in this repository. Do not open a public issue, and do not include cookies, signed addresses or private files in a report. You will get an answer as soon as possible; fixes ship as a new release that installed copies offer as an update.

Supported version: the latest [release](https://github.com/isolmaz/SSDownload/releases/latest).

## Security model

**Local only.** The app, its command line and the browser extension talk through a per-profile named pipe that only the current Windows user can open. Nothing listens on the network; the media tools reach the internet only through an authenticated loopback proxy owned by the app.

**Browser bridge.** The native messaging host accepts only the fixed extension ID `hgndggnlfpnflkmnbddmcnfniamckham`, length-limited JSON (1 MiB) and an allowlist of actions: add a download, analyze media and open the picker, renew a source address, relay a browser transfer, show the window and status queries. The browser can never choose an output path, delete files, change settings or quit the app.

**Secrets at rest.** Queue records, completion journals, site logins and the proxy password are sealed with Windows DPAPI for the current user. Cookies, headers and page markup are removed from a job once it completes. Logs and error reports strip credentials, query strings and fragments from addresses.

**Transfers.** HTTPS never downgrades on a redirect, and site headers or credentials never cross to another origin (only `Accept`, `Accept-Language` and `User-Agent` do). A download resumes only when the source proves it is unchanged. Files are published atomically, never replace an existing file and are never run automatically; downloaded files carry the Windows "downloaded from the internet" mark.

**Processes.** Helper programs are started with argument arrays, never through a shell, inside Windows job objects owned by the app, and they run only while a job needs them.

**Media tools.** yt-dlp, Deno and FFmpeg are downloaded over HTTPS from their GitHub releases, checked against the SHA-256 published with each release, installed atomically and checked again before each use. A failed refresh keeps the working set.

## Updates

- At most once a day the app reads `https://github.com/isolmaz/SSDownload/releases/latest/download/version.json` and its `version.json.sig`.
- The signature is Ed25519 over the exact bytes of `version.json`, verified against the public key embedded in the app (`FEED_PUBLIC_KEY` in [`src/update.rs`](src/update.rs)). The private key never enters this repository.
- The downloaded Setup must match the SHA-256 in the signed feed before it runs. Only newer `major.minor.patch` versions are offered, and nothing installs without your consent.
- The update flow never deletes files: superseded or rejected downloads move to `<profile>\update-quarantine\`.
- The Chrome extension's self-hosted update manifest (`update.xml`) and CRX are assets of the same releases; the extension ID is fixed by the `key` in `browser/chromium/manifest.json`.

The Windows binaries are not Authenticode-signed yet, so SmartScreen may warn when you run a downloaded copy. Compare its SHA-256 with the value on the release page.
