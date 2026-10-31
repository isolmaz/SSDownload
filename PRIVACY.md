# Privacy

SSDownload is a local Windows application. It has **no account, no sign-in, no cloud service, no telemetry, no analytics and no advertising**. There is no SSDownload server that receives your data. This page describes what the app and its browser extension actually do.

## What stays on your computer

- Your profile lives in `%LOCALAPPDATA%\SSDownload`: the download queue, settings, logs and the media tools.
- The queue, completion journals, site logins and the proxy password are encrypted with Windows DPAPI, so only your Windows account can open them.
- When a download completes, its cookies, request headers and page markup are removed from the stored record.
- Logs record addresses without credentials, query string or fragment. Their level, and whether site names are kept, are set in Settings.
- **Report problem** and **Tools → Diagnostics** write a folder and ZIP to your disk only when you ask. They are never uploaded; review them before sharing. A **Report problem** folder can hold the failing page's markup (`sayfa.html`) unredacted: the copy the browser handed over or, without one, the page read once by the app from its address without query or fragment, with no cookies, credentials or referer.

## Network connections

SSDownload connects only to:

- **The sources you download from**, through your proxy if you set one (FTP only through a SOCKS5 proxy; if the proxy cannot be used at start, the app warns and connects directly). Site credentials and headers are never forwarded to a different host after a redirect.
- **Digest files next to a download:** when you gave no SHA-256 and the source states no `Repr-Digest` for a file of at least 1 MiB, the app asks the server of the address you gave for at most two small files, `<file>.sha256` and `SHA256SUMS` in the file's folder, to verify the finished file. They go only to that server, never to another host a redirect led to, with the download's own request headers and your allowed session cookies for it; an answer over 64 KiB is discarded. Nothing about you is added to what the download request already sends.
- **GitHub, for updates:** at most once a day it reads the signed `version.json` of the latest [release](https://github.com/isolmaz/SSDownload/releases) over HTTPS. Turn this off in **Settings → General**. An update is downloaded and installed only after you accept it.
- **GitHub, for media support:** to install and weekly refresh the media tools from their GitHub releases over HTTPS — [yt-dlp](https://github.com/yt-dlp/yt-dlp), [Deno](https://github.com/denoland/deno) and the [BtbN FFmpeg builds](https://github.com/BtbN/FFmpeg-Builds). Each file must match the SHA-256 checksum published with that release.

GitHub, like any web server, sees the IP address of the computer that requests a file.

The installer also registers the extension with Chrome and Edge (`HKCU\Software\Google\Chrome\Extensions` and `HKCU\Software\Microsoft\Edge\Extensions`). The browser itself then downloads and updates the extension from the [Chrome Web Store](https://chromewebstore.google.com/detail/hgndggnlfpnflkmnbddmcnfniamckham), as it does for any store extension; SSDownload makes no connection for this. Uninstalling removes the entries and the browser removes the extension.

## Browser extension

- The extension sends data **only to the SSDownload app on the same computer**, through the browser's native messaging channel.
- When you start a download, it sends the download or page address, the request headers the browser used (Referer, Origin, Accept, Accept-Language, User-Agent) and, for a video handover, the page markup (used only for a local error report).
- No `Cookie` or `Authorization` header is copied from network observation. A site's cookies are read **only after you allow it for that site** (**Allow session cookies for this site**) and the permission can be withdrawn at any time.
- Taking over browser downloads is off by default; incognito downloads always stay in the browser.
- To offer a player's subtitles, the extension notes the address of a subtitle file (WebVTT or SRT) the page's player has already loaded; it does not fetch or keep the file's text.
- The extension contains no remote code and stores only its on/off state, the interface language the app reported (one of its seven languages), the sites where you hid the button, where you dragged the button on the player, the sites where you allowed session cookies and short-lived per-tab discovery state.

### Extension permissions

| Permission | Why |
|---|---|
| `nativeMessaging` | The only connection to the SSDownload desktop app (`com.ssdownload.desktop`); every download runs there. |
| `contextMenus` | The right-click entries for links and media. |
| `downloads` | Optional takeover of ordinary browser downloads (off by default). A download is removed from the browser list only after the app accepts it. |
| `tabs`, `webNavigation` | Identify the tab, frame and document a video belongs to, so the picker opens for the player you used. |
| `webRequest` | Notice media manifests (HLS/DASH) the page loads. Nothing is blocked or modified. |
| `storage` | The on/off state, the app's interface language, hidden-button sites, the button's position on the player, sites allowed to share session cookies and short-lived discovery state. |
| `alarms` | Resume an interrupted browser transfer and refresh the completed-downloads badge for a while after a handover. |
| `cookies` (optional) | Requested only when you allow session cookies for one site. |
| Access to `http://*/*` and `https://*/*` | Media discovery must run on the page you choose. Page data goes nowhere except the local app. |

## Website

The project site [ssdownload.isolmaz.com](https://ssdownload.isolmaz.com) counts anonymous visits with cookie-free Cloudflare Web Analytics and keeps your language and light/dark choice in your browser (`localStorage`).

## Deleting your data

- Uninstalling keeps your downloaded files and your profile.
- Deleting `%LOCALAPPDATA%\SSDownload` removes the queue, settings, logs and media tools.
- A `.ssdownload-work` folder inside a download folder holds unfinished jobs and can be deleted when you no longer need them.

Questions: open an [issue](https://github.com/isolmaz/SSDownload/issues). Security problems: see [SECURITY.md](SECURITY.md).
