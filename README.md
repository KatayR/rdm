# RDM

RDM (Rusty Download Manager) is a download manager in Rust: downloads over many connections
at once, resume, queues and a scheduler, categories, speed limits, the browser taking every
download, and video downloads from web pages. It runs on Windows today; macOS is planned (most
of the code already has its macOS side, but it has not been built on a Mac yet).

Its windows and the way it works will be familiar to those who know Internet Download
Manager. RDM is written from scratch and is not affiliated with Tonec; it contains none of
Internet Download Manager's code, icons or images. "Internet Download Manager" and "IDM" are
names of Tonec's product.

Status: version 0.3.0, the first under the name RDM and the first with a setup exe and its
own update. The setup is on the [releases page](https://github.com/KatayR/rdm/releases/latest);
what it does is under [The setup exe and updates](#the-setup-exe-and-updates).

- [Features](#features)
- [Repository layout](#repository-layout)
- [Building](#building)
- [Setting up](#setting-up)
- [The setup exe and updates](#the-setup-exe-and-updates)
- [The app](#the-app)
- [Command line](#command-line)
- [Data folder and settings](#data-folder-and-settings)
- [How it works](#how-it-works)
- [Testing](#testing)
- [Known gaps](#known-gaps)

## Features

**Downloading** (`engine`)
- Up to 32 connections per download (8 by default). The file is split while it downloads: a
  connection that runs out of work splits the range with the most bytes left.
- Connections open gradually (4 first, then up to twice as many as the requests the server has
  answered so far), and the download steps back when a server turns connections away (429 / 503, honouring
  `Retry-After` up to 60 s).
- A slow or stalled connection's rest is taken over by a faster one.
- Resume after a stop, a crash or a restart, when the server supports ranges (otherwise the file
  starts over). It is checked against the server (ETag / Last-Modified, and a reread of the end
  of what was written) so two versions of a file are never mixed.
- A file changed on the server during the download is caught (`If-Range`) and started over once;
  a second change fails the download.
- Whole-file hashes the server sends are checked before the file gets its name: `Repr-Digest`,
  `Digest`, `x-goog-hash`, Azure's `x-ms-blob-content-md5`, Backblaze's `X-Bz-Content-Sha1`,
  `X-Linked-ETag`, also on redirect hops; `Content-MD5` and `Content-Digest` only on a whole-file
  answer. None is used when the answer is content-encoded.
- Speed limits: one for everything and one per download, changed live.
- Retries with backoff per connection (5), redirects, the server's file name
  (`Content-Disposition`), file names made safe for Windows.
- Request headers from the browser (Referer, Cookie, User-Agent). Cookies and credentials are
  never sent on to another host after a redirect.

**Managing** (`manager`)
- A list of downloads kept in SQLite, with default categories (Compressed, Documents,
  Music, Programs, Video, General), each with its own folder.
- Queues: how many files at once, retries for the files the queue started (failed files are
  tried again at the queue's next start, dead links are not), a daily start and stop time,
  days of the week. The main queue exists from the start.
- Download File Info: the download starts while the window is still open, and moves
  if the folder or name is changed there.
- Download File Info notices an address already in the list, and offers to continue an
  unfinished download from a fresh address (for links that expire) without starting over.
- Names already taken in a folder get ` (1)`, ` (2)`, ...
- Checks after a download: zip-like files are unpacked in memory and every entry's CRC is
  checked; a video's length is compared with the expected one. These only warn.
- A hash typed in by hand (MD5, SHA-1, SHA-256 or SHA-512) can be checked against a finished
  file.

**Browser** (`extension`, `rdm-host`)
- Every http(s) download the browser starts goes to RDM, unless its file type is on the
  exclusion list (empty by default), it is Alt+clicked, or capture is switched off on the
  toolbar button. A download RDM refuses or does not answer stays in the browser (the button
  shows "!"); `blob:` and `data:` downloads always do.
- Plain clicks on links that look like files go to RDM before the browser acts on them; a
  link that turns out to be a web page is handed back.
- "Download with RDM" on links and "Download video with RDM" on pages and videos, in the
  right-click menu.
- A "Download this video" button over videos on any site: under the pointer, and without it
  while a video plays with sound (✕ hides it for that video).
- The toolbar button shows how many videos the tab has caught.
- Works in Brave and Chrome (Manifest V3).

**Video** (`manager`, using yt-dlp, Deno and ffmpeg)
- Qualities are listed by yt-dlp, grouped by resolution, with nothing chosen in advance. The
  streams the tab itself played (HLS, DASH, plain files) are listed too. When the page's
  list is those streams once more, only the streams are shown.
- An HLS or DASH playlist the page's own script read is listed by RDM from that text, at
  once and without asking its address again: this also works for playlists that answer once,
  only to a POST or only to the page, and for playlists the page builds itself.
- On YouTube the window opens with the list: the extension reads the formats YouTube's own
  player was given in the page (no request is made), and yt-dlp's list takes its place when
  it is in. yt-dlp is asked in advance once the video has played 1.5 s with sound (when
  capture is on, RDM is running and yt-dlp is installed).
- HLS and DASH are downloaded by RDM itself, piece by piece over several connections, with
  AES-128 decryption and resume.
- Video and audio are joined by ffmpeg without re-encoding (stream copy only). The joined
  file is opened again before it gets its name: if ffmpeg cannot read what it wrote, the join
  is made again as MKV.
- yt-dlp is kept up to date (checked daily, can be switched off); Deno is updated with Update
  now. Both downloads are checked against their release's checksums, and yt-dlp's also against
  its maintainers' signature. ffmpeg is RDM's own small build and comes with the programs.

**Interface** (`app`)
- The windows: main list, Download File Info, download progress (with the segment bar
  and connections), download complete, properties, Download Video, Options.
- A tray icon with the main queue and the speed limiter. Closing the main window keeps RDM
  running in the tray.

## Repository layout

| Path | What it is |
|---|---|
| `engine/` | Library: one download, either a file over many connections or a stream made of pieces (HLS / DASH). No list, no database. |
| `manager/` | Library: the download list, queues, scheduler, categories, settings, video pages, tools, and the local socket other programs talk to. |
| `cli/` | The `rdm` command and the browser's helper `rdm-host`. |
| `app/` | The Tauri 2 app `rdm-app`: owns the manager, draws the windows (`app/ui`, plain HTML / CSS / JS, no build step) and the tray icon. |
| `extension/` | The browser extension (Manifest V3). |
| `testserver/` | A local HTTP server for tests: throttled, cut or refused connections, HLS playlists, and so on. |
| `ffmpeg/` | Scripts for a minimal 4.2 MB ffmpeg (stream copy only), with its own [README](ffmpeg/README.md). |

### engine

| File | What it holds |
|---|---|
| `src/lib.rs` | Public API (`download`, `download_probed`, `download_pieces`, `probe`, `Options`, `Progress`, `Outcome`), the HTTP clients. |
| `src/probe.rs` | The first request (`Range: bytes=0-0`): size, range support, final address after redirects, file name, validators, hashes. |
| `src/download.rs` | The multi-connection download: workers, writing, the resume file, the final check. |
| `src/segments.rs` | The segment table (bookkeeping only, no I/O): who owns which range, splits and take-overs. |
| `src/pieces.rs` | Streams made of pieces: fetched ahead over several connections, written in order, resumable. |
| `src/hls.rs` | HLS media playlists (keys, byte ranges, init sections); refuses live and DRM streams. |
| `src/dash.rs` | DASH manifests from their text: the streams and their pieces (templates, timelines, lists, single files); refuses live, several periods and DRM. |
| `src/limiter.rs` | A token bucket shared by any number of connections and downloads. |
| `src/checksum.rs` | Hashes from server headers and the check of the finished file. |
| `src/error.rs` | Errors and the retry policy. |
| `tests/download.rs`, `tests/pieces.rs` | End-to-end tests against the test server. |

### manager

| File | What it holds |
|---|---|
| `src/lib.rs` | `Manager`, `Config`, the data folder (`home()`), errors. |
| `src/manager.rs` | The shared state, open and shutdown, the list, events and the once-a-second ticker. Its children in `src/manager/`: |
| `src/manager/run.rs` | Running one download: start, pause, placing the file, finishing. |
| `src/manager/streams.rs` | A video's streams, downloaded and joined by ffmpeg. |
| `src/manager/queues.rs` | Queues and starting what is waiting. |
| `src/manager/settings.rs` | Settings and category folders. |
| `src/manager/file_info.rs` | Download File Info: downloads started before the answer, moving them, duplicates, a new address. |
| `src/manager/files.rs` | Files on disk: free names, deleting, moving, the hash check. |
| `src/manager/video_pages.rs` | yt-dlp and co.: versions, updates, quality lists, listing ahead. |
| `src/store.rs` | SQLite: tables, migrations, defaults. |
| `src/model.rs` | Shared types (`Download`, `Status`, `Event`, `Queue`, `Settings`, ...). |
| `src/categories.rs` | The default categories and the extension wildcards. |
| `src/schedule.rs` | Queue times and days. |
| `src/filecheck.rs` | Checks after a download (zip CRCs, video length). |
| `src/video.rs` | yt-dlp calls, the quality list, turning a choice into streams. |
| `src/caught.rs` | What a tab played, sorted out; HLS and DASH playlists listed from the text the page read. |
| `examples/caught.rs` | Replays what the extension caught in a tab: what the Download Video window would offer, and a download of one line of it. |
| `src/media.rs` | ffmpeg: joining and remuxing with stream copy. |
| `src/tools.rs` | Installing and updating yt-dlp and Deno. |
| `src/tools/yt-dlp-signing-key.asc` | yt-dlp's release signing key, built into the program. |
| `src/ipc.rs` | The local socket: requests, replies, the daemon. |
| `tests/manager.rs`, `tests/daemon.rs` | End-to-end tests of the manager and of its socket; `tests/fixtures` holds a signed yt-dlp checksum list. |

### cli

| File | What it holds |
|---|---|
| `src/main.rs` | The `rdm` command and its subcommands. |
| `src/bin/host.rs` | `rdm-host`, the native messaging host the browser starts. |
| `src/lib.rs` | The modules both programs share: |
| `src/host.rs` | The host's message framing and handling. |
| `src/browser.rs` | `rdm browser install / uninstall`: the extension files and the host registration. |
| `src/remote.rs` | Connecting to the running manager, starting it if needed. |
| `src/get.rs` | `rdm get`, a download in the foreground without the manager. |
| `src/fmt.rs` | Sizes, rates and times for printing. |
| `tests/host.rs` | The host end to end: a framed message from the "browser" to a running manager. |

### app

| File | What it holds |
|---|---|
| `src/main.rs` | Start-up, single instance, the tray, opening windows on socket requests and download events. |
| `src/windows.rs` | Creating and placing each window. |
| `src/commands.rs` | The commands the windows call. |
| `src/update.rs` | RDM's own update: looking for a newer version, the question, fetching the setup and handing over to it. |
| `ui/` | One `.html` + `.js` per window (`main`, `info`, `progress`, `complete`, `properties`, `delete`, `video`, `options`, `update`), plus `common.js` and `style.css`. |
| `tauri.conf.json`, `capabilities/default.json`, `build.rs`, `icons/` | Tauri's configuration (with the update's address and public key), what the windows may do, the build script, the icons. |
| `tauri.bundle.json`, `installer/hooks.nsh` | What only the setup exe needs (merged in by `package.ps1`): the files to ship, and the setup's own steps. |

### extension

| File | What it holds |
|---|---|
| `manifest.json` | Permissions, scripts, and the `key` that fixes the extension's ID. |
| `background.js` | Taking browser downloads, the right-click menus, the toolbar switch, talking to the host. |
| `content.js` | Clicks on links that look like files; Alt+click. |
| `types.js` | The default list of file types for link clicks. |
| `catch.js` | Notes the HLS, DASH and media files each tab requests; the count on the toolbar button. |
| `rules.json` | What counts as media: types, extensions, skipped hosts, size floors. |
| `sniff.js` | Runs in the page: spots playlists in its fetch and XHR replies and in the Blobs it hands its player, and reads their text. On YouTube: the format list the player was given. |
| `video.js` | The "Download this video" button; listing YouTube qualities ahead. |

### testserver

| File | What it holds |
|---|---|
| `src/lib.rs` | The server: `/data`, `/redirect`, `/hls/...`, and their query parameters. |
| `src/main.rs` | Running it by hand. |
| `src/folder.rs` | `Folder`, a temporary folder of its own for each test. |

## Building

Needs:
- Rust stable (built with 1.99.0) with the MSVC toolchain.
- Visual Studio Build Tools with the C++ workload and a Windows SDK.
- WebView2 (built into Windows 11), for the app.

```
cargo build --release -p rdm -p rdm-app
```

This gives `target\release\rdm.exe`, `rdm-host.exe` and `rdm-app.exe`. The three must
stay in the same folder: the command starts the app next to it, and the browser registration
points to the host next to it. None of them needs the Visual C++ runtime installed (it is
linked in: by tauri-build for the app, by `cli/build.rs` for the other two).

ffmpeg is looked for in the same folder (`ffmpeg.exe` beside the programs), which is where
an installed RDM has it. For work from the build folder, set `RDM_FFMPEG` to the build made
by `ffmpeg/` (`target\ffmpeg-min\work\ffmpeg-win64\ffmpeg.exe`) or copy that file next to
the programs. Without it everything works except videos whose streams have to be joined or
repacked (those fail and say where ffmpeg belongs), and the length check of a finished video
is skipped.

The app has no frontend build step: `app/ui` is served as it is. `cargo build` makes no
setup exe (`bundle.active: false` in `app/tauri.conf.json`); `package.ps1` does, see
[The setup exe and updates](#the-setup-exe-and-updates).

The minimal ffmpeg is built separately, in WSL, as described in [ffmpeg/README.md](ffmpeg/README.md).

## Setting up

1. **Start the app**: run `target\release\rdm-app.exe`. It opens the main window and puts an
   icon in the tray. Only one runs per data folder; starting it again shows the running one.
   `--background` starts it with the tray icon only. `rdm` commands that need the manager
   start the app in the background when nothing is running.
2. **Register the browser helper**:
   ```
   target\release\rdm browser install
   ```
   This writes the extension's files (built into `rdm.exe`) to `%APPDATA%\rdm\extension`,
   writes the host manifest `%APPDATA%\rdm\com.rdm.host.json` (with the full path of
   `rdm-host.exe`), and registers it for the current Windows account only (HKCU) for Chrome
   and Brave: `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.rdm.host` and
   `HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.rdm.host`.
3. **Load the extension**: open `brave://extensions` (or `chrome://extensions`), switch on
   Developer mode, choose Load unpacked and pick `%APPDATA%\rdm\extension`. Its ID must read
   `hmkendmmfmfkmjcocflneimlmlgjbdmm` (fixed by the `key` in `manifest.json`); the browser lets
   no other extension start the host.
4. **Install the video tools**: Options → Tools → Update now, or `rdm tools update`. This
   installs yt-dlp and Deno into `%APPDATA%\rdm\tools`. Plain downloads need neither.

After a rebuild that changes the extension, or after moving the program folder, run
`rdm browser install` again and reload the extension in the browser. When the version number
changed with it, the extension reloads itself: each time the browser wakes it, it reads the
manifest in its folder, and when that names another version than the one running, it loads
the new files (once nothing is on its way to RDM). Pages open at that moment keep the old
copy's scripts until they are loaded again: a file link in them is then the browser's own
download, which the new copy takes, and the video button says to reload the page.

`rdm browser uninstall` removes the registration and the host manifest. The extension's files
stay in `%APPDATA%\rdm\extension`, and the extension is removed from the browser by hand.

## The setup exe and updates

`RDM_<version>_x64-setup.exe` installs RDM for the current Windows account, without
administrator rights, into `%LOCALAPPDATA%\RDM`: the three programs, `ffmpeg.exe`, and the
licences (`licenses\`). It then runs `rdm browser install` itself, so steps 1 and 2 of
[Setting up](#setting-up) are done; the extension is still loaded by hand (step 3, from
`%APPDATA%\rdm\extension`) and the video tools are fetched from Options (step 4). The setup is
not code-signed: Windows warns before running it ("Windows protected your PC", More info, Run
anyway), and a PC with Smart App Control on refuses it.

Uninstalling (Windows' Installed apps) takes the browsers' registration away with the
programs. The list, the tools and the extension's files stay in `%APPDATA%\rdm` unless
"Delete the application data" is ticked.

### Updates

An installed app looks for a newer version when it starts and then once a day (Options,
About: the check box, and "Check for updates"). A copy run from the build folder does not:
there is nothing a setup would replace. It reads
`https://github.com/KatayR/rdm/releases/latest/download/latest.json`; a newer version opens a
window with what is new and **Update now** / **Later**. Update now:

1. fetches that version's setup and checks its signature against the public key built into
   the app (`plugins.updater.pubkey` in `app/tauri.conf.json`). The signature also names the
   version it was made for, so an old setup cannot be passed off as a new one. A setup that
   does not check out is not run, and RDM stays as it was;
2. closes RDM down as Exit does: the socket first, then every download (they continue after
   the update), so the setup finds none of RDM's programs at work;
3. starts the setup in its update mode (a progress bar, no questions), which puts the new
   files in place, runs `rdm browser install` and starts RDM again.

An update copies the new files over the old ones; it uninstalls nothing. What keeps that from
going wrong:

- The setup first makes the browsers forget the helper (which they would otherwise start,
  and it the app, in the middle of the copying), then closes the app, the helper, the command
  line and ffmpeg of its folder (`app/installer/hooks.nsh`): a running program cannot be
  replaced. After the copying the browsers are told again.
- Should a program stay behind as the old version all the same, the programs refuse to work
  together and say why (see [The manager](#the-manager)).
- An older RDM does not open a list a newer one wrote, and the setup refuses to install an
  older version over a newer one.
- The extension loads its new files by itself (see [Setting up](#setting-up)).
- A file a new version no longer ships stays on disk; nothing starts it. One that must go is
  deleted by name in `hooks.nsh`.

### Making a release

1. Set the version in `Cargo.toml` (`[workspace.package]`) and in `extension/manifest.json`
   (a test fails when they differ), and in this README's status line.
2. `.\package.ps1 -Notes "what is new"` builds `rdm.exe` and `rdm-host.exe`, the list of the
   libraries' licences (cargo-about, `about.toml`), then the setup with Tauri's bundler
   (`cargo tauri build` with `app/tauri.bundle.json` merged in), signs it and writes
   `latest.json`. All three files are in `target\release\bundle\nsis`.
3. A GitHub release named `v<version>` gets the setup, its `.sig`, `latest.json`, and
   `ffmpeg-9.0.2.tar.xz` (ffmpeg's source has to be offered where its program is). Installed
   RDMs find it through the `latest` release.

The update key is `%USERPROFILE%\.tauri\rdm-update.key` (or `TAURI_SIGNING_PRIVATE_KEY`),
outside the repository; its public half is `plugins.updater.pubkey` in `app/tauri.conf.json`.
**Without the key no installed RDM can be given an update, ever**: it has to be kept in a
second place. A fork that makes setups of its own needs its own pair (`cargo tauri signer
generate`), with the public half and its own address (`endpoints`) in `app/tauri.conf.json`.

A release is tried before it is public by installing a build whose update address is the PC itself
(`-Config` with a file that sets `plugins.updater.endpoints` to `http://127.0.0.1:<port>/latest.json`
and `dangerousInsecureTransportProtocol`), and serving the next version's three files there
(`-Url` gives `latest.json` the local address of the setup).

## The app

- **Main window**: categories on the left (All Downloads, each category, Unfinished, Finished),
  the list on the right with File Name, Size, Status, Time left, Transfer rate, Last Try Date and
  Description. The columns can be resized and sorted, and both are remembered.
  - Toolbar: Add URL, Resume, Stop, Stop All, Delete, Options, and Start Queue / Stop Queue for
    the main queue.
  - Right-click menu: Open, Open the containing folder, Resume / Stop, Copy address, Delete,
    Properties. Delete (the button, the menu or the Delete key) asks in a window of its own,
    which names what is selected: Remove from list (Enter), Remove and delete files, or
    Cancel (Escape).
  - A double click opens a finished file, or the progress window of an unfinished one.
  - Add URL sends web pages (HTML or XHTML) to the Download Video window and anything else to
    Download File Info, also a page the server sends as a file to save. A link clicked in the
    browser is judged the same way.
- **Download File Info** (for downloads from the browser and from Add URL, unless switched off):
  address, category, Save As with Browse, description, size and whether it can resume; Start
  Download, Download Later (with a queue), Cancel. The download already runs while the window is
  open; closing it unanswered removes it. An address already in the list waits for the answer
  and offers "Show it" / "Continue it"; the same file under a new address offers "Use this
  address for it". With the window switched off, a download from the browser or from Add URL
  starts at once.
- **Download progress**: status, size, downloaded, transfer rate, time left, resume capability;
  Show details adds the segments and the connections. Pause / Resume and Cancel (which pauses).
  It opens when a download starts (if switched on) and always when one fails.
- **Download complete**: Open, Open folder, Close, the hash check and the file check;
  "Don't show this dialog again".
- **Properties**: address, path, category, size, status, dates, the checks, an editable
  description, and, once the file is complete, a box for checking a hash.
- **Download Video**: the qualities yt-dlp lists for the page, then the streams the tab played,
  then plain files (the one the clicked video plays first); what the tab played before a
  single-page site moved on comes last, marked "earlier in this tab"; category, Save As,
  description, Download Later (with a queue), Start Download, Cancel.
- **Options**:
  - General: show Download File Info / the progress window / the complete window, and the file
    types the browser keeps.
  - Connection: connections per download (1 to 32) and the total speed limit.
  - Save To: a folder for each category.
  - Queues: pick one or make a new one; name, files at once, retries, start and stop times, days.
  - Tools: the installed versions, "Keep yt-dlp up to date (checked daily)", Update now.
  - About: RDM's version, its licence and where its source is, "Look for a new version of
    RDM once a day", Check for updates.
- **Tray**: Open RDM, Options, Speed limiter (switches the limit on and off and remembers its
  value; with no value yet it opens Options), Start / Stop main queue, Exit.

Only Exit in the tray or `rdm shutdown` quits. Downloads running at that moment continue on
the next start.

## Command line

`rdm` talks to the running app (or a headless manager) over a local socket and starts the app
in the background when needed. `get`, `formats`, `tools`, `browser` and `daemon` run on their
own; `shutdown` never starts anything.

| Command | What it does |
|---|---|
| `get <url> [-c N] [-o DIR] [-n NAME]` | Downloads one file in the foreground, without the manager. Ctrl+C stops it; the same command again resumes. |
| `add <url>` | Adds a download and starts it now, outside the queue's limit. `--later` puts it in a queue instead (`--queue ID`, the main queue by default). Also `-o DIR`, `-n NAME`, `-c N`, `--referer`, `--cookie`, `-H "Name: value"` (repeatable), `--limit 500K`, `--description`, `--video FORMAT`. No Download File Info window is shown. |
| `formats <url> [--referer] [-H ...]` | Lists a video page's or playlist's qualities, with the format to pass to `add --video`. |
| `list` (`ls`) | The list: id, status, done, size, speed, connections, time left, name, the last error, and "may be damaged" when a file check failed. |
| `pause <ids>` / `resume <ids>` | Stops or continues downloads; `resume` runs them outside their queue's limit. |
| `remove <ids> [--delete]` (`rm`) | Removes downloads; `--delete` also deletes finished files (partial ones always go). |
| `move <ids> --queue ID` | Moves downloads to the end of a queue. |
| `limit [RATE\|off]` / `limit RATE --id ID` | Shows or sets the total speed limit, or sets one download's. |
| `connections N` | Connections per download, 1 to 32. |
| `queue [list]` / `queue new NAME` / `queue start ID` / `queue stop ID` | Queues. |
| `queue set ID` | `--name`, `--parallel`, `--retries`, `--start HH:MM\|off`, `--stop HH:MM\|off`, `--days mon,tue,...\|all`. |
| `categories [NAME DIR]` | Lists categories, or sets one's folder. |
| `browser install` / `browser uninstall` | See [Setting up](#setting-up). |
| `tools [list]` / `tools update [--nightly\|--stable]` | Shows or updates yt-dlp and Deno, in that order, stopping at the first that fails. yt-dlp stays on its channel (as with the app's Update now); `--nightly` switches it to its nightly builds, `--stable` back. |
| `daemon` | Runs the manager in the console, without windows, until Ctrl+C. |
| `shutdown` | Stops the manager; reports an error if none runs. |

Rates are bytes per second in binary units: `500K` is 500 × 1024, `2M` and `1.5M` are
mebibytes, `G` works too, and a plain number is bytes. `off` or `0` removes a limit.

## Data folder and settings

Everything lives in one folder: `%APPDATA%\rdm` on Windows, `~/Library/Application
Support/rdm` on macOS. The `RDM_HOME` environment variable puts it elsewhere (each data
folder gets its own socket, so two can run side by side, which the tests rely on). A manager
started in the background by a command or by the browser helper is given the folder of
whoever started it.

| File | What it is |
|---|---|
| `rdm.db` | SQLite (WAL): tables `downloads`, `categories`, `queues`, `settings`, `playlists` (the playlist texts of videos picked from what a tab played). |
| `rdm.db.before-update` | The list as it was before a newer RDM last changed its form (one copy, replaced at the next such change). |
| `catch-rules.json` | Optional, written by hand: the person's own rules for what the extension catches, used instead of the built-in `extension/rules.json` (same shape). |
| `tools/` | yt-dlp, Deno, yt-dlp's cache and `tools.json` (versions, last check). |
| `extension/`, `com.rdm.host.json` | Written by `rdm browser install`. |
| `daemon.log`, `app.log`, `host.log` | Logs of a manager started in the background, of the app (panics), and of the browser helper (requests by type and address only; cookies are never written). |

Partial files sit next to their target as `<name>.part`, with the resume state in
`<name>.part.json`. A video's streams are downloaded as `<stem>.d<id>.f<format>.<ext>` and
joined into the final file.

Settings (kept as one JSON row in `settings`):

| Setting | Default | Meaning |
|---|---|---|
| `connections` | 8 | Connections per download, 1 to 32. |
| `speed_limit` | 0 | Total speed limit in bytes per second; 0 is none. |
| `speed_limit_value` | 0 | The limit the tray's Speed limiter switches back on. |
| `downloads_dir` | the system Downloads folder | The folder the categories point to when the database is first created (see below). |
| `ask_before_download` | true | Show Download File Info for new downloads. |
| `show_progress` | true | Open the progress window when a download starts. |
| `show_complete` | true | Open the complete window when a download finishes. |
| `exclude_types` | none | File types the browser keeps. |
| `auto_update_tools` | true | Check yt-dlp for updates daily. |
| `check_for_updates` | true | Look for a new version of RDM at the start and once a day (an installed app only). |

Categories (extension wildcards `*` and `?`):

| Category | Extensions |
|---|---|
| Compressed | zip rar r0\* r1\* arj gz sit sitx sea ace bz2 7z |
| Documents | doc pdf ppt pps docx pptx |
| Music | mp3 wav wma mpa ram ra aac aif m4a tsa |
| Programs | exe msi |
| Video | avi mpg mpe mpeg asf wmv mov qt rm mp4 flv m4v webm ogv ogg mkv ts tsv |
| General | everything else |

When the database is first created, General's folder is `downloads_dir` and the others are
subfolders of it named after them (made when the first file goes there). A file no category
matches goes to General. Category folders are changed in Options → Save To or with
`rdm categories`; nothing changes `downloads_dir` itself.

The main queue ("Main download queue") and new queues start with 1 file at a time and 10
retries. Schedules use local time; a start or stop time that passed while the computer slept is
still acted on.

## How it works

### One download (engine)

1. **Probe**: a GET for the first byte. Redirects are followed by hand (up to 10) so hash
   headers on each hop are seen. A 206 with a known size means ranges work; otherwise one
   connection downloads the file whole, and a stop means starting over.
2. **Segments**: the `.part` file is made at full size and every connection writes at its own
   offset. A free connection first takes a released range, otherwise it splits the range with
   the most bytes left at the middle of what is left (if both halves are at least 1 MiB).
3. **Take-over**: an idle connection takes the whole unwritten rest of a range when its owner
   wrote nothing for 3 s, or has held it at least 3 s and its recent speed is under half the
   asker's. Each time the same range is taken over again the wait doubles (up to 96 s), and a
   connection waiting on the speed limit is never taken over.
4. **Writing**: each connection buffers up to 256 KiB and writes at least once a second. Once a
   second the data is flushed and the resume state is saved through a temporary file and a
   rename, so the state never claims bytes that are not on disk.
5. **Resume**: the state is used only if the size, the `.part` length and the validator still
   match, and if the last 64 KiB (or less) of the longest written run read back the same from
   the server.
6. **Finish**: the file is hashed if the server gave a hash, then renamed from `.part`.

Pieces (HLS / DASH): up to twice as many pieces as connections are fetched ahead of the file's
end and written strictly in order. The first piece in line is asked for once more on a free
connection once its request is 3 s old and it is not waiting on the speed limit, if nothing
arrived for it for 3 s or every piece behind it came in more than twice as fast. Whichever
copy finishes first is kept. TS pieces disguised behind a fake image header are
unwrapped. A resume downloads the last piece written again and starts over if it changed.

### The manager

- One `Manager` holds the list in memory behind one lock and writes every change through to
  SQLite. Lock order: the state, then the store; no lock is held across an await. A change to
  settings, categories or queues, and a removal, take effect only once saved; a failed save of a
  download is only logged: the list in memory stays the truth and the next save writes it.
- A ticker runs once a second: speeds, progress (saved every 5 s), queue schedules.
- Queues start waiting downloads in order, up to their "files at once". A download its queue
  started goes back to waiting after a failure until the queue's retries are used up. A
  download started by hand (Start Download, `add`, Resume) runs outside the queue's limit and
  gets no queue retries: once the engine's own retries are used up, it fails.
- Each start of a queue (by hand or by its schedule) gives its files their retries afresh and
  puts the failed ones back to waiting, so a queue run after a stop, or on the next day, tries
  them again. Paused files stay paused. A move to a queue also gives fresh retries.
- A file whose server answers its first request with "not there" (404 Not Found, 410 Gone)
  has failed for good: its queue spends no retries on it and a queue start leaves it failed,
  so a dead link is not tried every morning. Resume, a fresh address from the browser, or
  Download Later tries it again. Other refusals (403 and the like: an expired link, but also
  a daily quota or a block lifted later) and a "not there" partway through a download are
  retried as before. Video downloads never count as failed for good, since their stream
  addresses are fetched again at every start.
- The app and `rdm daemon` serve the same local socket (a named pipe on Windows, `daemon.sock`
  on macOS), one JSON request per line. The CLI and the browser helper work with either, except
  that choosing a video quality and showing windows need the app.
- The programs of one installation have one version, and say it: a connection's first line is
  the caller's version, and a manager of another version refuses it ("RDM was updated only in
  part ... run its setup again") instead of carrying out requests that may mean something else
  to it. That is what an update leaves when one program was running and could not be replaced.
- A list written by a newer RDM is not opened by an older one (the app says so in a message
  box and ends; nothing in the file is touched). Before a newer RDM changes an existing list's
  form it copies it to `rdm.db.before-update`.

### From the browser

The extension never talks to the app directly. It calls the native messaging host
`com.rdm.host`; the browser starts `rdm-host.exe`, which relays the request over the
socket, starting the app if needed. There is no open network port. For a download, the host
probes the address with the browser's cookies, referrer and user agent first, and refuses web
pages that came from a link click so the browser can open them. The extension waits for the
answer for at most 30 s and leaves the download to the browser if RDM does not take it.

### Video

The extension notes HLS, DASH and media files each tab requests (read only). A reply is judged
by the file name the server gives, then the extension in its address, then its type. What a
media element asked for counts however small (except sounds under 100 KiB); what scripts fetch
counts from 512 KiB. One file met under several addresses (mirrors, or a counter in the
address: same name, same size to the byte) is noted once. In the page itself, the extension spots playlists in fetch and XHR replies
whatever their address and type say, and in Blobs the page turns into `blob:` addresses, and
sends a playlist's whole text along (HLS and DASH; up to 2 MiB).

On "Download this video", yt-dlp lists the page's qualities (with Deno for YouTube's player),
and the program sorts out what the tab played: a quality's playlist belongs to its master, a
playlist's pieces are no files of their own, two playlists with the same text are one. A
playlist with text is listed by the program itself (`caught.rs`, with the readers in
`engine/src/hls.rs` and `engine/src/dash.rs`); one without (only the network log saw it), and
one the program does not read (a live one, a DASH manifest with several periods), is listed
by yt-dlp from its address. yt-dlp names the playlist each of its formats comes from: when
all of the page's list comes from playlists the tab played, the page's list is left out and
the streams stay (they carry what the browser sent for them and, when the page read them,
their text). A DASH picture is paired with sound in the language the tab loaded, else the one
the manifest marks as main, else its first.

On YouTube the page itself holds the list of formats its player was given (sizes and codecs,
no addresses). The extension reads it from the player and sends it with the button's message;
the program makes the usual quality list from it (`video::from_player`) and the window shows
that until yt-dlp's own list arrives. A line can be picked from it at once; Start then waits
for yt-dlp's list (a moment at most) and takes the same line from there, because yt-dlp's
names for the formats, and the formats it can get, are not always the page's.

On every start a page's chosen quality is resolved again by yt-dlp, since stream addresses
expire. A video picked from a playlist's text keeps that text (table `playlists`) and is
resolved from it; the pieces, and the playlists of qualities the page did not play, are still
fetched by the program. Plain streams are downloaded by the engine as files and playlists as
pieces; ffmpeg then joins video and audio with stream copy into MP4, WebM or MKV.

The rules for what the extension catches are one list, `extension/rules.json`, built into the
extension and into the helper. The extension asks the helper for them when it starts (the
`rules` message, which does not start RDM) and uses the helper's when they are at least as
new, or when they are the person's own (`catch-rules.json` in the data folder).

### Tool updates

yt-dlp (`yt-dlp/yt-dlp`, or its nightly builds) and Deno (`denoland/deno`) come from their
GitHub releases. Each file is checked against its SHA-256 from the release (yt-dlp's checksum
list, Deno's file of its own), and yt-dlp's list must carry a valid signature from its
maintainers' key (embedded in `manager/src/tools/`). Zip paths are kept inside the target
folder. yt-dlp's folder is replaced as a whole and Deno's single file by a rename, so nothing
is left half-updated. ffmpeg is no such tool: it is RDM's own build, ships beside the programs
and changes only with RDM itself, so the two are always the pair that was tested together.
While the app or `rdm daemon` runs, it looks every hour and updates yt-dlp once its last
check is a day old, if yt-dlp is installed and "Keep yt-dlp up to date" is on.

## Testing

```
cargo clippy --workspace --all-targets
cargo test --workspace
```

- Tests use `testserver` on a free local port and a temporary folder of their own each, so test
  runs at the same time do not meet.
- Eight tests need ffmpeg, most of them two kinds. What RDM itself does is run with the
  ffmpeg RDM ships: where `RDM_FFMPEG` points, else `target\ffmpeg-min\work\ffmpeg-win64\ffmpeg.exe`
  (the build made by `ffmpeg/`). The sample videos and the packet hashes are made with a full
  ffmpeg, which the shipped one cannot do (no encoders): where `RDM_FFMPEG_FULL` points, else
  the `ffmpeg` on the PATH. A test that lacks one fails and says so, rather than pass without
  checking anything.
- The test server can also be run by hand: `cargo run -p testserver -- 8090 256` serves 256 MiB
  at `http://127.0.0.1:8090/data`. Query parameters shape the answer: `rate` (bytes per second
  per connection), `ranges=0`, `cut`, `busy`, `limit`, `crawl`, `cookie`, `name`, and so on; and
  `/hls/index.m3u8?pieces=N&size=N` serves playlists (`key`, `map`, `disguise`, `slow`, `fail`,
  `live`). See `testserver/src/lib.rs` for all of them.
- The windows have no automated tests; they are checked by hand.

## Known gaps

- **macOS**: not built or run on a Mac yet.
- **Linux**: not supported; `cli/src/browser.rs` has no Linux code.
- **Browsers**: Brave and Chrome only. No Firefox or Safari.
- **Sites behind Cloudflare's bot check** may refuse RDM even with the browser's cookies,
  since its TLS fingerprint is not a browser's.
- **The setup exe** is not code-signed (Windows warns; Smart App Control refuses it), does not
  bring the extension into the browser (it is loaded unpacked, with Developer mode on), and
  has no "start with Windows". Run while the helper, the command line or ffmpeg of RDM's
  folder is at work, it waits about 30 s for each before Windows ends it by force (an update
  from the app does not meet that: the app has stopped them).
- **Tools**: no lock against two updates at once, and a new yt-dlp is not test-run before it
  replaces the old one.
- **Queues and categories**: queues cannot be deleted or reordered, and no categories can be
  added. A download that was running when RDM quit continues on the next start outside its
  queue.
- **Video**: no audio track or language choice for HLS and DASH (one track is taken), no
  live recording, no subtitles, and cookies are not sent to piece or key hosts other than the
  playlist's (streams behind signed CDN cookies fail). Playlists read inside a Web Worker are
  only seen by their address. A DASH manifest with several periods is left to yt-dlp. An
  `<audio>` element gets no button (the toolbar count and the right-click menu cover it).
- Download File Info shows nothing while a long move runs.
- While `rdm daemon` serves the data folder, the app does not start (it finds the socket
  taken and exits).
