# SSDownload guide

Everything beyond the [README](../README.md): installation details, every menu, setting and shortcut, recovery and the command line. A new installation starts in the Windows display language (Turkish on a Turkish Windows, English otherwise) and can be switched in **Settings → General**; the command line and the browser extension follow it. Menu names below are given in English with the Turkish label in parentheses where it helps.

- [Install](#install)
- [Browser extension](#browser-extension)
- [Downloading](#downloading)
- [Menus and shortcuts](#menus-and-shortcuts)
- [Settings](#settings)
- [Recovery](#recovery)
- [Error codes and logs](#error-codes-and-logs)
- [Command line](#command-line)
- [Data and uninstall](#data-and-uninstall)

## Install

- **Installer:** run `SSDownload-<version>-Setup.exe`. It installs per user (no administrator rights) into `%LOCALAPPDATA%\Programs\SSDownload`, adds a Start menu entry and registers the browser connection. Optionally it starts SSDownload with Windows.
- **Portable:** extract `SSDownload-<version>-win64.zip` to a permanent folder, run `register-native-host.cmd` once for the browser connection and open `ssdownload.exe`. Moving the folder later requires running `register-native-host.cmd` again.
- **Before upgrading,** exit through the tray menu. Installed copies update themselves: SSDownload checks the signed release feed at most once a day (Settings → General) and installs a verified update after you accept it (**Install**, **Later** or **Skip this version**; Esc means Later). If a silent install fails, the app reopens and reports why on its next start. A portable copy is not changed in place: the offer opens the release page so you can download the new ZIP.
- **Media support** (yt-dlp, FFmpeg/ffprobe and Deno) is downloaded by the installer (shown there as "missing components"); a portable copy, an installer that could not reach the network, or an automatic update (silent) installs anything missing on the app's next start instead. It comes from the GitHub releases of yt-dlp, Deno and the BtbN FFmpeg builds, each file checked against its published SHA-256, and refreshes weekly. A failure never blocks startup; **Tools → Install media support** retries.
- The builds are not Authenticode-signed yet, so Windows SmartScreen may warn on first run. Compare the file with the SHA-256 on the release page before running it.

## Browser extension

The Chrome/Edge extension adds the overlay button on videos, the right-click entries and optional download takeover. It talks only to the desktop app on the same computer. Its own text (button, popup, menus and messages) is English or Turkish and follows the app's interface language (**Settings → General**); until the app has reported it (not installed, not running, an older version), the browser's language decides: Turkish in a Turkish browser, English otherwise.

- **Automatic:** Setup (and `register-native-host.cmd` for the portable ZIP) registers the [Chrome Web Store extension](https://chromewebstore.google.com/detail/hgndggnlfpnflkmnbddmcnfniamckham) with Chrome and Edge for your Windows user. The next time the browser starts it shows a "New extension added" notice: click **Enable extension**. Close every browser window first if the notice does not appear. If you remove the extension in the browser, it stays removed. Uninstalling SSDownload removes the registration, and the browser then removes the extension.
- **Settings → General → Install the extension in browsers** repeats the registration and opens the Chrome Web Store page, where **Add to Chrome** installs it at once. It also removes the force-install policy entries that versions up to 1.6.1 wrote, because Chrome blocks the extension while one exists. If those entries need administrator rights, the app asks first (`ssdownload-cli --remove-extension-policies`).
- **Without the store:** open `chrome://extensions` (or `edge://extensions`), turn on **Developer mode**, choose **Load unpacked** and select the `browser\chromium` folder next to `ssdownload.exe` (installed copies: `%LOCALAPPDATA%\Programs\SSDownload\browser\chromium`). **Tools → Browser setup** opens that folder and the steps. The bundled copy has the same extension ID, so the browser connects it to the app the same way.
- **Toolbar popup:** the discovery switch (`ON`/`OFF` in English, `AÇIK`/`KAPALI` in Turkish), **Hide the button on this site** (discovery and context entries stay), the page's media list (each entry opens the desktop picker) and **Open the app**. When the app cannot be reached, the popup says why. After a handover the icon badge counts completed downloads. Browsing alone never starts SSDownload: only an action you take (download, picker, Open the app) does.
- **Video button menu:** the arrow at the right end of the button on a video opens **Hide on this video**, **Hide on this page** and **Hide on this site**. The first two last until the page is reloaded or left; the site choice is the popup's **Hide the button on this site**, where it is turned back on.
- **Extension without the app** (installed from the store first, or on macOS/Linux): it adds no video button, no context entries and takes over no downloads; browser downloads continue as usual. The popup says the app was not found and its main button becomes **Download the app**, which opens the download page. SSDownload itself runs on Windows only.
- If the browser cannot connect, check that the app and extension versions match and register the copy you use (`register-native-host.cmd` or reinstall).

## Downloading

- **Files:** paste or drop links into **New download** (Ctrl+N), press Ctrl+V in the main window to add clipboard links, right-click a link in the browser → **Download link with SSDownload**, or let the extension take over ordinary browser downloads (**Settings → Behaviour → Take over browser downloads**, off by default; incognito, blob/data and extension downloads stay in the browser and a refused handover resumes there).
- **Segmented and resumable:** large files download over several connections. A download resumes only when the server proves the file did not change (strong ETag, a SHA-256 you supplied, or the same `Last-Modified` and length); otherwise it restarts instead of mixing two versions. FTP follows the same rule with the server's size and modification time; a server that does not report both, or refuses to resume, restarts the download. A resume record that exists but cannot be read (for example while a virus scanner holds it) fails the attempt, keeps the partial files and is retried. A supplied SHA-256 that does not match deletes the partial file, so a retry downloads it again. Finished files are published atomically and never overwrite an existing file (`name (1).ext`).
- **Video and audio:** play the media and press the SSDownload button over it, use the extension's context entries, or use **File → Video download** (Ctrl+Shift+N) with the page address. The picker shows name, duration, thumbnail, quality (container, resolution, FPS, codec, HDR) and audio/subtitle tracks. **Download** turns the picker into that download's status view; closing it never cancels the download. A missing quality or track fails instead of being silently substituted, and the result is checked with ffprobe/FFmpeg before it is published.
- **Streams with separate video and audio:** a finished, unencrypted HLS source whose video and audio come as separate playlists downloads both at once in the app, each segment retried on its own; a server that answers "too many requests" gets fewer parallel requests until it recovers, and a paused download continues after the segments it already has. The streams are then copied into one file without re-encoding. Every other source shape downloads through yt-dlp.
- **Picker memory:** **Remember this choice for \<site\>** stores quality, container and audio-only per site; the next media from that site is queued directly. **MP3** queues the best audio as MP3 at the highest quality; a playlist accepts an item selection such as `1-10,15`.
- **Live streams** (Twitch, Kick and similar) are recorded from the moment the download starts until the stream ends; the list shows the recorded size and speed instead of a percentage. A paused live recording cannot continue the same file.
- **From the stream's beginning:** for a live stream the picker offers **Download from the stream's beginning**. Where the site keeps the running stream from its start (yt-dlp supports this on YouTube and Twitch, for example), the recording begins there and continues while the stream runs; elsewhere (Kick, for example) the stream is recorded from now on and the status says so.
- **Finish live recording and keep it** (Downloads menu, or the right-click menu of a recording row) stops a running live video recording and publishes what was recorded so far as a normal file, without its last second (the frame being written when it stopped).
- **On sites with feeds or previews,** open the video's own page first. DRM-protected streams are not supported.
- **Batch:** a numeric or letter range such as `https://host/img[001-120].jpg` expands into up to 1000 jobs; **File → Batch download** shows an example, **Import list** loads a plain-text URL list and **Export list** writes the selected addresses.
- **Duplicates:** adding an address already in the queue asks once per batch: use the existing job, download a new copy, or cancel.
- **Session cookies:** some downloads need the site's session. Grant **Allow session cookies for this site** in the extension for that one site; nothing is read without that gesture, and **Remove site session permission** withdraws it.
- **Expired links:** a job whose signed address expired waits as **Awaiting source**; **Refresh source address** (or the extension's browser transfer entry) resumes the same job with a fresh address. **Pause all** and **Start now** leave such a job waiting; **Pause all** also cancels a refresh still running for it.
- **Order and priority:** drag rows or use Alt+Home / Alt+End; within a queue, queued work starts in list order, and several queues take turns. **Start now** starts a job immediately, outside the concurrency limit.
- **Mini panel and completion card:** **Downloads → Mini panel** (or the tray) opens a small list of active downloads that can be pinned on top. A finished download shows a card with **Open** and **Show in folder**.
- **Tray:** progress on the icon; the menu has the mini panel, new download, add from clipboard, pause/resume all, speed-limit presets, shut down when done, a one-hour quiet mode, the download folder, updates, Settings and Exit.
- **Holds:** a job waits when its disk has less than 256 MiB (or less than the remaining size) free, and optionally while Windows reports a metered connection. Windows is kept awake while downloads run (the display may still sleep).
- **Basic and Advanced** (`Basit` / `Gelişmiş`) are chosen in the first-run wizard and in Settings (**Interface level**). Advanced adds named queues with time windows and byte quotas, folder rules, a site crawler, periodic synchronization, site logins, the event and job logs and completion actions (notify, shut down, run a program). An action runs once every job of its queue has completed; a failure in the queue or removing one of its jobs cancels the action until new work is added there. Shutting down and running a program also wait until every job in the list has completed (a failed, paused or cancelled job in another queue holds them back until it is retried or removed), and only one countdown runs at a time; cancelling it also cancels the other shutdown and program actions waiting behind it.

## Menus and shortcuts

| Menu | Items |
|---|---|
| **File** (Dosya) | New download (Ctrl+N), Video download (Ctrl+Shift+N), Batch download, Import list, Export list, Exit |
| **Downloads** (İndirme) | Pause / resume selected (Space), Remove selected (Delete), Pause all, Resume all, Clear completed, Retry failed, Mini panel, Open file (Enter), Open folder, Refresh source address, Finish live recording and keep it, Speed limit, Select all (Ctrl+A) |
| **Tools** (Araçlar) | Install media support, Browser setup, Settings; on the Advanced level also Queue manager, Folder rules, Site explorer, Sync manager, Site logins, Event log, Job log, Diagnostics |
| **Help** (Yardım) | Check for updates, Keyboard shortcuts, Show the tour, About SSDownload |

The progress column fills from the left as a job advances: blue while it runs, grey while it waits or is paused, green when it completed and red when it failed or was cancelled (those two fill the whole cell).

Other shortcuts: Ctrl+V adds clipboard links, Ctrl+F searches, F5 refreshes, Alt+Home / Alt+End move the selection to the top or bottom, and **Ctrl+Shift+D adds the clipboard link from anywhere** (Settings → Behaviour). Double-clicking a completed row opens the file or its folder. Removing several jobs asks once: **Yes** also deletes the files, **No** keeps them.

## Settings

| Tab | Controls |
|---|---|
| **General** | Interface level (Basic or Advanced) and interface language (Turkish or English; the browser extension follows it); download folder; usage profiles; clipboard detection; start with Windows; minimize to tray; completion notification; dark title bar and controls; site entries in the browser; daily working hours; update check; extension install. |
| **Advanced** | Concurrent jobs (1–16); connections per file (1–16); media fragment concurrency (1–16); per-host jobs and connections (1–16); global speed limit (KiB/s, 0 = unlimited); retry count (0–20); experimental browser transfer; event log level and host recording. |
| **Network** | Proxy: the Windows setting, none, or manual `http://` / `socks5://` with an optional user name and password; a scheduled speed limit (daily window and KiB/s). |
| **Behaviour** | Keep Windows awake while downloading; completion sound; completion card; browser download takeover; hold on a metered connection; the Ctrl+Shift+D hotkey; double-click action; history retention in days (0 keeps everything). |

The Basic level shows only **General**; the other tabs keep their saved values.

- **Speed limit:** **Downloads → Speed limit** sets KiB/s for the selected jobs (0 follows the global limit); the tray presets set the global limit. A running live recording keeps the limit it started with, because changing it would restart the capture.
- **Proxy:** the Windows setting is read at start and when the proxy setting is changed in Settings, and only its HTTP/HTTPS entry is used (no automatic configuration script and no SOCKS-only entry). FTP downloads go through a SOCKS5 proxy only; with an HTTP proxy they connect directly. An HTTP proxy password is sent with Basic authentication. If the proxy cannot be used at start (for example its saved password cannot be read), the app shows a warning and connects directly.
- **Site logins** (Advanced): host → user name and password, sent as HTTP Basic credentials only to that exact host over HTTPS (or HTTP when the entry allows it). Letter case, an international spelling and a trailing dot do not make a different host, and a host has one login. A redirect to another host never carries them.
- **Scheduling** uses local time; the start is inclusive, the end exclusive, `23:00–07:00` crosses midnight and equal endpoints mean all day. Manually paused work never resumes by itself.
- **Profiles:** data lives in `%LOCALAPPDATA%\SSDownload`; `--data-dir <folder>` or the `SSDOWNLOAD_HOME` environment variable selects another profile. The most recently registered copy owns the browser connection.
- **Developer mode:** `--debug` or `SSDOWNLOAD_DEBUG=1` adds **Media tools** and **Diagnostics** and shows raw component output; `SSDOWNLOAD_DEBUG=0` turns it off again.

## Recovery

100% transfer progress is not completion: merging and final validation still run, and only **Completed** unlocks **Open file**. Failed jobs keep their data, except a partial file whose supplied SHA-256 did not match.

| Symptom | What to do |
|---|---|
| The browser cannot connect | Make sure the app and extension versions match; register the copy you use. |
| A second launch while SSDownload runs | The running copy comes to the front; no second instance starts. If that copy is still closing, the new one starts as soon as it has exited. |
| Download stays disabled after analysis | Read the error, fix the source or session, analyze again. |
| A quality or track disappeared | Analyze again and choose an available option; nothing is substituted silently. |
| Signed address expired | The job waits for **Refresh source address** instead of failing. |
| Merge fails or a work folder remains | Free space and resume; parts stay under `<destination>\.ssdownload-work\jobs\<job-id>`. |
| Remove hits a sharing violation | Close the program holding the file and retry. |
| The queue cannot be saved | Active jobs pause and new work waits while saving is retried. |
| Encrypted records do not open | Records are bound to the Windows user (DPAPI). Unreadable ones move to quarantine and return when the profile is opened under the original account again. |
| Database from a newer version | An older build refuses a newer database instead of rewriting it. |

## Error codes and logs

- Classified failures carry permanent `SSD-<area>-<number>` codes with Turkish and English messages. Areas: `BRG` browser bridge, `MED` media resolution, `NET` network, `TRF` transfer, `STO` storage, `RCV` recovery, `TOL` media tools, `UPD` updates, `EXT` extension. Codes are never renumbered or reused.
- Logs live in `<profile>\logs\`: `events-YYYYMMDD.jsonl` and `ssdownload-YYYYMMDD.log`, rotating at 5 MB with five kept and removed after 30 days, plus per-job timelines in `logs\jobs\`. The browser connection appends its refusals to the same files (channel `native-host`) and leaves rotation to the app; it writes nothing when it cannot read the profile settings. URLs are logged without credentials, query or fragment; with **Log site names** off, site names are hidden as well, also in error messages that repeat the download's site or name a host after `host:`, `host=` or `connect to`.
- **Report problem** (on a failed analysis or job) and **Tools → Diagnostics** write a local folder and ZIP. Nothing is uploaded; look through it before sharing it in an issue. A **Report problem** folder can include the failing page's markup as `sayfa.html`, unredacted: the copy the browser handed over or, without one, the page read once by the app (address without query or fragment, no cookies, credentials or referer).

## Command line

`ssdownload-cli.exe` (next to `ssdownload.exe`) forwards commands to the running app and prints JSON for scripts. `ssdownload-cli --help` lists everything:

| Command | Purpose |
|---|---|
| `--add URL [--kind auto\|file\|video\|audio] [--output DIR] [--name FILE]` | Queue a download; also `--connections 1..16`, `--sha256 HEX`, `--container mp4\|mkv`, `--max-height 1080`, `--format ID`, `--audio-format best\|mp3\|m4a\|opus\|flac`, `--subtitle LANG`, `--playlist`, `--referer URL`, `--start-at TIME`, `--request-id ID` (repeating it returns the same job) |
| `--request-json FILE`, `--action-json FILE` | Queue a download from an AddRequest JSON file; run an action JSON file (for example a site crawl) |
| `--inspect URL` | Print the media analysis as JSON |
| `--status [--status-offset 0 --status-limit 100]` | Paged queue status |
| `--pause ID`, `--resume ID`, `--remove ID [--delete-file]`, `--pause-all`, `--resume-all`, `--clear-completed` | Queue control |
| `--diagnose [--diagnose-hours 24]` | Event log summary |
| `--set-settings FILE` | Save the settings from a Settings JSON file |
| `--install-tools [--update-tools]` | Install or refresh the verified media tools |
| `--setup-tools` | Install missing media tools without starting the app (used by the installer) |
| `--register-host`, `--unregister-host` | Browser connection registration |
| `--remove-extension-policies`, `--browser-setup` | Remove the legacy extension policy; open the browser setup page |
| `--enable-autostart`, `--disable-autostart` | Start with Windows |
| `--background`, `--data-dir DIR`, `--debug`, `--quit`, `--version` | Start options |

## Data and uninstall

- Uninstalling removes the program, its shortcuts and browser registration, and **keeps your downloads and your profile**.
- Deleting `%LOCALAPPDATA%\SSDownload` removes the queue, settings, logs and media tools. A `.ssdownload-work` folder in a download folder holds unfinished jobs.
- See [PRIVACY.md](../PRIVACY.md) for what is stored and where.
