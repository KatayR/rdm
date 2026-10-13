//! Types shared by the store, the manager and the socket protocol.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// In a queue, waiting for the queue to start it.
    Waiting,
    Downloading,
    /// Stopped by hand; queues skip it until it is resumed.
    Paused,
    Complete,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Waiting => "waiting",
            Status::Downloading => "downloading",
            Status::Paused => "paused",
            Status::Complete => "complete",
            Status::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        [Status::Waiting, Status::Downloading, Status::Paused, Status::Complete, Status::Failed]
            .into_iter()
            .find(|st| st.as_str() == s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    /// Grows with every new download and is never reused.
    pub id: i64,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Chosen by hand, or learned from the server when the download starts.
    pub file_name: Option<String>,
    /// Chosen by hand, or the category's folder when the download starts.
    pub dir: Option<PathBuf>,
    pub category: Option<String>,
    pub queue: i64,
    /// Order inside the queue.
    pub position: i64,
    pub size: Option<u64>,
    /// `size` is a guess: a playlist without byte ranges, measured on the pieces so far.
    #[serde(default)]
    pub size_estimated: bool,
    pub downloaded: u64,
    pub status: Status,
    /// None: the default from the settings.
    pub connections: Option<u16>,
    /// Bytes per second for this download alone; None or 0 means no own limit.
    pub speed_limit: Option<u64>,
    /// Unix seconds.
    pub added: i64,
    pub last_try: Option<i64>,
    /// Failed runs inside a queue, counted against the queue's retries.
    pub attempts: u32,
    pub error: Option<String>,
    /// The last run failed because the server said there is no file at the address (404,
    /// 410): queues leave it until it is started again by hand or given a new address.
    #[serde(default)]
    pub failed_for_good: bool,
    /// Final file once complete.
    pub path: Option<PathBuf>,
    pub description: Option<String>,
    /// Set for a video page: yt-dlp's format choice (`299+140`, `bv*+ba/b`). The page is
    /// resolved to stream addresses at every start, since those expire after hours.
    #[serde(default)]
    pub video: Option<String>,
    /// What the finished file was checked against ("SHA-256 from the server (Repr-Digest)").
    #[serde(default)]
    pub checked: Option<String>,
    /// The check by file type after completion (zip contents, video length).
    #[serde(default)]
    pub file_check: Option<FileCheck>,
    /// That check is running; not stored.
    #[serde(default)]
    pub checking: bool,
    /// Started while Download File Info is still open, as IDM does: not in the list and
    /// without windows of its own until the person answers (stored, so one left by a
    /// crash is cleaned up on the next start).
    #[serde(default)]
    pub provisional: bool,
    /// The server answered once and the download was given its file name
    /// (`Inner::place`): from then on the name is its own, and for a plain file
    /// `<name>.part` too. Before that, `file_name` is only the name asked for, which
    /// another download may hold.
    #[serde(default)]
    pub placed: bool,
    /// The server takes ranges (resume capability); known once it answered. Not stored.
    #[serde(default)]
    pub resumable: Option<bool>,
    /// The address was just replaced (an expired link given again from the browser): the
    /// next start accepts the part file under it. Not stored.
    #[serde(default)]
    pub new_address: bool,
    /// Live values while running; not stored.
    #[serde(default)]
    pub speed: u64,
    #[serde(default)]
    pub active_connections: usize,
    #[serde(default)]
    pub segments: Vec<engine::Segment>,
    /// A playlist download: `(written, all)` pieces.
    #[serde(default)]
    pub pieces: Option<(usize, usize)>,
}

/// What happened to a download, for windows that follow it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "id", rename_all = "snake_case")]
pub enum Event {
    Started(i64),
    Complete(i64),
    Failed(i64),
    /// Paused, or sent back to waiting by its queue.
    Stopped(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    pub name: String,
    /// Lower-case extensions without dots; `*` and `?` work as in IDM (`r0*`).
    pub extensions: Vec<String>,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queue {
    pub id: i64,
    pub name: String,
    /// Files downloaded at the same time.
    pub parallel: u32,
    /// Extra runs for a file that failed.
    pub retries: u32,
    pub schedule: Schedule,
    pub running: bool,
}

/// Daily start and stop times in local time ("HH:MM").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub start: Option<String>,
    pub stop: Option<String>,
    /// Bit 0 = Monday ... bit 6 = Sunday; 0 means every day.
    pub days: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub connections: u16,
    /// Bytes per second for all downloads together; 0 means unlimited.
    pub speed_limit: u64,
    /// General downloads go here; other categories go to subfolders.
    pub downloads_dir: PathBuf,
    /// Windows of the app, as IDM's options; all on by default.
    #[serde(default = "yes")]
    pub ask_before_download: bool,
    #[serde(default = "yes")]
    pub show_progress: bool,
    #[serde(default = "yes")]
    pub show_complete: bool,
    /// File types the browser keeps (lower case, no dot); everything else goes to RDM.
    #[serde(default)]
    pub exclude_types: Vec<String>,
    /// The limit the speed limiter switches back on with; `speed_limit` is 0 while off.
    #[serde(default)]
    pub speed_limit_value: u64,
    /// Check yt-dlp for a new release once a day and install it (sites change, and an old
    /// yt-dlp stops finding their videos).
    #[serde(default = "yes")]
    pub auto_update_tools: bool,
    /// Look for a newer RDM once a day and offer it. The app does; a bare `rdm daemon`
    /// has nobody to offer it to.
    #[serde(default = "yes")]
    pub check_for_updates: bool,
}

impl Settings {
    /// True when the browser should keep a file of this name.
    pub fn excludes(&self, file_name: &str) -> bool {
        let ext = std::path::Path::new(file_name).extension().and_then(|e| e.to_str()).unwrap_or("");
        !ext.is_empty() && self.exclude_types.iter().any(|t| t.eq_ignore_ascii_case(ext))
    }
}

fn yes() -> bool {
    true
}

/// What `add` takes; everything but the URL is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewDownload {
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub file_name: Option<String>,
    pub dir: Option<PathBuf>,
    pub queue: Option<i64>,
    /// Start at once (IDM's "Start download") instead of waiting in the queue.
    #[serde(default)]
    pub start: bool,
    pub connections: Option<u16>,
    pub speed_limit: Option<u64>,
    pub description: Option<String>,
    /// Download the video on this page in this yt-dlp format (see `Download::video`).
    #[serde(default)]
    pub video: Option<String>,
    /// Let the person confirm it first (IDM's Download File Info) when an interface runs.
    #[serde(default)]
    pub ask: bool,
    /// Start at once while the person is still asked (see `Download::provisional`).
    #[serde(default)]
    pub provisional: bool,
    /// For a video picked from a playlist the browser's page read: that playlist first,
    /// then the ones it names whose text is known. `video` is then a format of the first
    /// one's own listing (`caught::resolve`) and yt-dlp is not asked.
    #[serde(default)]
    pub playlists: Vec<Playlist>,
}

/// An HLS or DASH playlist as the browser's page read it. Kept with its download: some addresses
/// answer once, or only to the page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    /// Where the page read it; a `blob:` address for one the page built itself.
    pub url: String,
    pub text: String,
}

/// "Download this video" in the browser: the page (or the player's frame) and what its tab
/// played, for the Download Video window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoPage {
    pub url: String,
    /// The tab's title, for the file name when yt-dlp does not know the page.
    pub title: Option<String>,
    /// What the browser sends for the page: User-Agent, Cookie, and the tab's page as
    /// Referer when this is a player's frame.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub caught: Vec<Caught>,
    /// What the page's own player was given, when the extension could read it.
    #[serde(default)]
    pub player: Option<PlayerData>,
}

/// The formats a page's own player was given (YouTube puts them into the page), as the
/// extension read them: enough to show the quality list at once. No addresses are among
/// them; yt-dlp finds those when the download starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerData {
    /// The video's id on the site.
    pub id: String,
    pub title: Option<String>,
    pub seconds: Option<f64>,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub formats: Vec<PlayerFormat>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerFormat {
    /// The site's number for the format, which is also yt-dlp's name for it.
    pub itag: u32,
    /// `video/mp4; codecs="avc1.640028"`.
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    /// Bits per second.
    pub bitrate: Option<f64>,
    /// Bytes.
    pub size: Option<u64>,
    /// `1080p60 HDR`.
    pub quality: Option<String>,
    /// A quieter copy of an audio track ("dynamic range compressed").
    #[serde(default)]
    pub drc: bool,
    /// The audio track's name, when the video has several.
    pub track: Option<String>,
    /// The track the video was made with.
    #[serde(default)]
    pub original: bool,
    /// The track the player starts with for this viewer.
    #[serde(default)]
    pub preferred: bool,
    #[serde(default)]
    pub drm: bool,
}

/// What the check by file type found; `ok` false is a warning, the file stays complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileCheck {
    pub ok: bool,
    pub text: String,
}

/// An outside program for video downloads, as installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    /// None when not installed.
    pub version: Option<String>,
    pub nightly: bool,
}

/// What Update now did for one program.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUpdate {
    pub name: String,
    pub ok: bool,
    /// "2026.08.19 installed", "2026.08.19 is the newest", or the error.
    pub text: String,
}

/// A finished file's hash, and whether it is the one asked about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HashCheck {
    /// "SHA-256"
    pub kind: String,
    /// Lower-case hex.
    pub hash: String,
    /// None when no hash was given to compare with.
    pub matches: Option<bool>,
}

/// A media response the browser saw a tab receive (IDM's way of finding video on any site).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caught {
    pub url: String,
    pub kind: CaughtKind,
    /// Bytes, when the server said.
    pub size: Option<u64>,
    /// What the browser sent: Referer, Origin, Cookie, User-Agent.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// A playlist's text, when the page's own script read it.
    #[serde(default)]
    pub text: Option<String>,
    /// Caught before the tab's address last changed without a page load (a single-page
    /// site moved on): probably another video's.
    #[serde(default)]
    pub earlier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaughtKind {
    /// An HLS playlist (.m3u8).
    Hls,
    /// A DASH manifest (.mpd).
    Dash,
    /// A plain video or audio file.
    File,
}
