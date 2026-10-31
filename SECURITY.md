# Security

## Reporting a vulnerability

Please report security problems **privately** through GitHub: **Security → Report a vulnerability** in this repository. Do not open a public issue, and do not include cookies, signed addresses or private files in a report. You will get an answer as soon as possible; fixes ship as a new release that installed copies offer as an update.

Supported version: the latest [release](https://github.com/isolmaz/SSDownload/releases/latest).

## Security model

**Local only.** The app, its command line and the browser extension talk through a per-profile named pipe that only the current Windows user can open. Clients open it at the identification level and write nothing until the process serving it is confirmed to run as the same user, so a pipe another user created under that name receives nothing. Nothing listens on the network; the media tools reach the internet only through an authenticated loopback proxy owned by the app.

**Browser bridge.** The native messaging host accepts only the fixed extension ID `hgndggnlfpnflkmnbddmcnfniamckham`, length-limited JSON (1 MiB) and an allowlist of actions: add a download, analyze media and open the picker, renew a source address, relay a browser transfer, show the window and status queries. The browser can never choose an output path, delete files, change settings or quit the app.

**Secrets at rest.** Queue records, completion journals, site logins and the proxy password are sealed with Windows DPAPI for the current user. Cookies, headers and page markup are removed from a job once it completes. Logs and error reports strip credentials, query strings and fragments from addresses. The page markup a **Report problem** folder may hold (`sayfa.html`) is kept as is, so review it before sharing.

**Transfers.** HTTPS never downgrades on a redirect, and site headers or credentials never cross to another origin (only `Accept`, `Accept-Language` and `User-Agent` do). A file download that a redirect ends on a web page which the server does not send as an attachment (a sign-in page, for example) fails instead of saving the page, unless the file's name is a page's. A download resumes only when the source proves it is unchanged, and only from bytes its resume record lists as flushed to disk (a record that an earlier version wrote for a one-connection download resumes from the length of the part file). Files are published atomically, never replace an existing file and are never run automatically; downloaded files carry the Windows "downloaded from the internet" mark.

**Published digests.** When you gave no SHA-256, a finished HTTP file is checked against the SHA-256 its source publishes: the `Repr-Digest` header of the response that delivered it or, when that states none and the file is at least 1 MiB, a `.sha256` or `SHA256SUMS` file next to the address you gave. The digest comes from the same source as the bytes, so this detects damage in transit, on a mirror or in a truncated body, not a malicious or compromised server, which can publish the digest of whatever it serves. Where that matters, give the SHA-256 you obtained from a separate trusted channel; it always takes precedence and nothing extra is requested. The two small files are read from the origin of the address you gave (same scheme, host and port, even when the download was redirected elsewhere) with the download's own request headers, which never go to another origin; redirects to another origin are not followed, at most two requests and 15 seconds are used for each download of the file (four requests when it is downloaded again, as below), a lookup that finds no free connection of the host in that time is skipped, and a body over 64 KiB, a web page or an unclean line is ignored. A file that does not match is deleted and downloaded once more. If the second copy has the very bytes of the first, transfer damage is ruled out and the digest is stale, describes another file, or the server's own copy is damaged, which the app cannot tell apart; the file is then kept, as it would be with no digest, and the job carries a note naming the source of the digest. If the second copy differs from the first and does not match the digest either, the download fails with `SSD-TRF-003` and the file is never published. The used re-download, with the SHA-256 of the first copy, is recorded in the job's work folder, so closing the app does not grant another.

**Processes.** Helper programs are started with argument arrays, never through a shell, inside Windows job objects owned by the app, and they run only while a job needs them.

**Media tools.** yt-dlp, Deno and FFmpeg are downloaded over HTTPS from their GitHub releases, checked against the SHA-256 published with each release, installed atomically and checked again before each use. A failed refresh keeps the working set. yt-dlp may open `file:` addresses only in the manifests the app pinned itself; a site's media description that names a `file:` address, such as a network share, is refused.

## Updates

- At most once a day the app reads `https://github.com/isolmaz/SSDownload/releases/latest/download/version.json` and its `version.json.sig`.
- The signature is Ed25519 over the exact bytes of `version.json`, verified against the public key embedded in the app (`FEED_PUBLIC_KEY` in [`src/update.rs`](src/update.rs)). The private key never enters this repository.
- The downloaded Setup must match the SHA-256 in the signed feed before it runs. Only newer `major.minor.patch` versions are offered, and nothing installs without your consent.
- The update flow never deletes files: superseded or rejected downloads move to `<profile>\update-quarantine\`.
- The browser extension is distributed through the Chrome Web Store; Chrome and Edge install and update it from there. Its ID is fixed by the `key` in `browser/chromium/manifest.json`, and the store item keeps that ID.

The Windows binaries are not Authenticode-signed yet, so SmartScreen may warn when you run a downloaded copy. Compare its SHA-256 with the value on the release page.
