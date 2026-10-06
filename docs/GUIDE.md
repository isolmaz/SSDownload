# SSDownload guide

Everything beyond the [README](../README.md): installation details, every menu, setting and shortcut, recovery and the command line. The interface is Turkish by default; menu names below are given in English with the Turkish label in parentheses where it helps.

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
- **Before upgrading,** exit through the tray menu. Installed copies update themselves: SSDownload checks the signed release feed at most once a day (Settings → General) and installs a verified update after you accept it.
- **Media support** (yt-dlp, FFmpeg/ffprobe and Deno) installs itself on first start from the GitHub releases of yt-dlp, Deno and the BtbN FFmpeg builds, each file checked against its published SHA-256, and refreshes weekly. A failure never blocks startup; **Tools → Install media support** retries.
- The builds are not Authenticode-signed yet, so Windows SmartScreen may warn on first run. Compare the file with the SHA-256 on the release page before running it.

## Browser extension

The Chrome/Edge extension adds the overlay button on videos, the right-click entries and optional download takeover. It talks only to the desktop app on the same computer. Its own text (button, popup and messages) is currently Turkish only.

- **Load unpacked (any device):** open `chrome://extensions` (or `edge://extensions`), turn on **Developer mode**, choose **Load unpacked** and select the `browser\chromium` folder next to `ssdownload.exe` (installed copies: `%LOCALAPPDATA%\Programs\SSDownload\browser\chromium`). **Tools → Browser setup** opens that folder and the steps.
- **Managed devices:** **Settings → General → Install the extension in browsers** (or `ssdownload-cli --install-extension`) writes the per-user force-install policy; the browser must fully restart. Chrome only honours this policy on domain-joined or Chrome Browser Cloud Management enrolled devices, so on other machines the app explains why it skipped the write.
- **Toolbar popup:** the discovery switch (`ON`/`OFF`), **Hide the button on this site** (discovery and context entries stay), the page's media list (each entry opens the desktop picker) and **Open the app**. After a handover the icon badge counts completed downloads.
- If the browser cannot connect, check that the app and extension versions match and register the copy you use (`register-native-host.cmd` or reinstall).

## Downloading

- **Files:** paste or drop links into **New download** (Ctrl+N), press Ctrl+V in the main window to add clipboard links, right-click a link in the browser → **Download link with SSDownload**, or let the extension take over ordinary browser downloads (**Settings → Behaviour → Take over browser downloads**, off by default; incognito, blob/data and extension downloads stay in the browser and a refused handover resumes there).
- **Segmented and resumable:** large files download over several connections. A download resumes only when the server proves the file did not change (strong ETag, a SHA-256 you supplied, or the same `Last-Modified` and length); otherwise it restarts instead of mixing two versions. Finished files are published atomically and never overwrite an existing file (`name (1).ext`).
- **Video and audio:** play the media and press the SSDownload button over it, use the extension's context entries, or use **File → Download video** (Ctrl+Shift+N) with the page address. The picker shows name, duration, thumbnail, quality (container, resolution, FPS, codec, HDR) and audio/subtitle tracks. **Download** turns the picker into that download's status view; closing it never cancels the download. A missing quality or track fails instead of being silently substituted, and the result is checked with ffprobe/FFmpeg before it is published.
- **Picker memory:** **Remember this choice for \<site\>** stores quality, container and audio-only per site; the next media from that site is queued directly. **MP3** queues the best audio as MP3; a playlist accepts an item selection such as `1-10,15`.
- **On sites with feeds or previews,** open the video's own page first. DRM-protected streams are not supported.
- **Batch:** a numeric or letter range such as `https://host/img[001-120].jpg` expands into up to 1000 jobs; **File → Batch download** shows an example, **Import list** loads a plain-text URL list and **Export list** writes the selected addresses.
- **Duplicates:** adding an address already in the queue asks once per batch: use the existing job, download a new copy, or cancel.
- **Session cookies:** some downloads need the site's session. Grant **Allow session cookies for this site** in the extension for that one site; nothing is read without that gesture, and **Remove site session permission** withdraws it.
- **Expired links:** a job whose signed address expired waits as **Waiting for source**; **Renew source address** (or the extension's browser transfer entry) resumes the same job with a fresh address.
- **Order and priority:** drag rows or use Alt+Home / Alt+End; queued work starts in list order. **Start now** starts a job immediately, outside the concurrency limit.
- **Mini panel and completion card:** **Download → Mini panel** (or the tray) opens a small always-on-top list of active downloads. A finished download shows a card with **Open** and **Show in folder**.
- **Tray:** progress on the icon; the menu has the mini panel, new download, add from clipboard, pause/resume all, speed-limit presets, shut down when done, a one-hour quiet mode, the download folder, updates, Settings and Exit.
- **Holds:** a job waits when its disk has less than 256 MiB (or less than the remaining size) free, and optionally while Windows reports a metered connection. Windows is kept awake while downloads run (the display may still sleep).
- **Simple and Advanced** (`Basit` / `Gelişmiş`) are chosen in the first-run wizard and in Settings. Advanced adds named queues with time windows and byte quotas, folder rules, a site crawler, periodic synchronization, site logins, the event and job logs and completion actions (notify, shut down, run a program).

## Menus and shortcuts

| Menu | Items |
|---|---|
| **File** (Dosya) | New download (Ctrl+N), Download video (Ctrl+Shift+N), Batch download, Import list, Export list, Exit |
| **Download** (İndirme) | Pause / resume selected (Space), Remove selected (Delete), Pause all, Resume all, Clear completed, Retry failed, Mini panel, Open file (Enter), Open folder, Renew source address, Speed limit, Select all (Ctrl+A) |
| **Tools** (Araçlar) | Install media support, Browser setup, Settings; on the Advanced surface also Queue manager, Folder rules, Site crawler, Synchronization, Site logins, Event log, Job log, Diagnostics |
| **Help** (Yardım) | Check for updates, Keyboard shortcuts, Show the tour, About SSDownload |

Other shortcuts: Ctrl+V adds clipboard links, Ctrl+F searches, F5 refreshes, Alt+Home / Alt+End move the selection to the top or bottom, and **Ctrl+Shift+D adds the clipboard link from anywhere** (Settings → Behaviour). Double-clicking a completed row opens the file or its folder. Removing several jobs asks once: **Yes** also deletes the files, **No** keeps them.

## Settings

| Tab | Controls |
|---|---|
| **General** | Simple/Advanced surface and interface language (Turkish or English); download folder; usage profiles; clipboard detection; start with Windows; minimize to tray; completion notification; dark title bar and controls; site entries in the browser; daily working hours; update check; extension install. |
| **Advanced** | Concurrent jobs (1–16); connections per file (1–16); media fragment concurrency (1–16); per-host jobs and connections (1–16); global speed limit (KiB/s, 0 = unlimited); retry count (0–20); experimental browser transfer; event log level and host recording. |
| **Network** | Proxy: the Windows setting, none, or manual `http://` / `socks5://` with an optional user name and password; a scheduled speed limit (daily window and KiB/s). |
| **Behaviour** | Keep Windows awake while downloading; completion sound; completion card; browser download takeover; hold on a metered connection; the Ctrl+Shift+D hotkey; double-click action; history retention in days (0 keeps everything). |

The Simple surface shows only **General**; the other tabs keep their saved values.

- **Speed limit:** **Download → Speed limit** sets KiB/s for the selected jobs (0 follows the global limit); the tray presets set the global limit.
- **Site logins** (Advanced): host → user name and password, sent as HTTP Basic credentials only to that exact host over HTTPS (or HTTP when the entry allows it). A redirect to another host never carries them.
- **Scheduling** uses local time; the start is inclusive, the end exclusive, `23:00–07:00` crosses midnight and equal endpoints mean all day. Manually paused work never resumes by itself.
- **Profiles:** data lives in `%LOCALAPPDATA%\SSDownload`; `--data-dir <folder>` or the `SSDOWNLOAD_HOME` environment variable selects another profile. The most recently registered copy owns the browser connection.
- **Developer mode:** `--debug` or `SSDOWNLOAD_DEBUG=1` adds **Media tools** and **Diagnostics** and shows raw component output; `SSDOWNLOAD_DEBUG=0` turns it off again.

## Recovery

100% transfer progress is not completion: merging and final validation still run, and only **Completed** unlocks **Open file**. Failed jobs keep their data.

| Symptom | What to do |
|---|---|
| The browser cannot connect | Make sure the app and extension versions match; register the copy you use. |
| A second launch while SSDownload runs | The running copy comes to the front; no second instance starts. |
| Download stays disabled after analysis | Read the error, fix the source or session, analyze again. |
| A quality or track disappeared | Analyze again and choose an available option; nothing is substituted silently. |
| Signed address expired | The job waits for **Renew source address** instead of failing. |
| Merge fails or a work folder remains | Free space and resume; parts stay under `<destination>\.ssdownload-work\jobs\<job-id>`. |
| Remove hits a sharing violation | Close the program holding the file and retry. |
| The queue cannot be saved | Active jobs pause and new work waits while saving is retried. |
| Encrypted records do not open | Records are bound to the Windows user (DPAPI). Unreadable ones move to quarantine and return when the profile is opened under the original account again. |
| Database from a newer version | An older build refuses a newer database instead of rewriting it. |

## Error codes and logs

- Classified failures carry permanent `SSD-<area>-<number>` codes with Turkish and English messages. Areas: `BRG` browser bridge, `MED` media resolution, `NET` network, `TRF` transfer, `STO` storage, `RCV` recovery, `TOL` media tools, `UPD` updates, `EXT` extension. Codes are never renumbered or reused.
- Logs live in `<profile>\logs\`: `events-YYYYMMDD.jsonl` and `ssdownload-YYYYMMDD.log`, rotating at 5 MB with five kept, plus per-job timelines in `logs\jobs\`. URLs are logged without credentials, query or fragment.
- **Report failure** (on a failed analysis or job) and **Tools → Diagnostics** write a local folder and ZIP. Nothing is uploaded; look through it before sharing it in an issue.

## Command line

`ssdownload-cli.exe` (next to `ssdownload.exe`) forwards commands to the running app and prints JSON for scripts. `ssdownload-cli --help` lists everything:

| Command | Purpose |
|---|---|
| `--add URL [--kind auto\|file\|video\|audio] [--output DIR] [--name FILE]` | Queue a download; also `--connections 1..16`, `--sha256 HEX`, `--container mp4\|mkv`, `--max-height 1080`, `--format ID`, `--audio-format best\|mp3\|m4a\|opus\|flac`, `--subtitle LANG`, `--playlist`, `--referer URL`, `--start-at TIME` |
| `--inspect URL` | Print the media analysis as JSON |
| `--status [--status-offset 0 --status-limit 100]` | Paged queue status |
| `--pause ID`, `--resume ID`, `--remove ID [--delete-file]`, `--pause-all`, `--resume-all`, `--clear-completed` | Queue control |
| `--diagnose [--diagnose-hours 24]` | Event log summary |
| `--install-tools [--update-tools]` | Install or refresh the verified media tools |
| `--register-host`, `--unregister-host` | Browser connection registration |
| `--install-extension`, `--remove-extension-policies`, `--browser-setup` | Extension policy and setup |
| `--enable-autostart`, `--disable-autostart` | Start with Windows |
| `--background`, `--data-dir DIR`, `--debug`, `--quit`, `--version` | Start options |

## Data and uninstall

- Uninstalling removes the program, its shortcuts and browser registration, and **keeps your downloads and your profile**.
- Deleting `%LOCALAPPDATA%\SSDownload` removes the queue, settings, logs and media tools. A `.ssdownload-work` folder in a download folder holds unfinished jobs.
- See [PRIVACY.md](../PRIVACY.md) for what is stored and where.
