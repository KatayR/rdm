//! Video pages and streaming playlists: yt-dlp turns a page (or an HLS / DASH playlist
//! address the browser caught) and a format choice into stream addresses, which the engine
//! then downloads: plain files like any file, playlists piece by piece.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::media;
use crate::tools::{Tool, Tools};
use crate::{Error, Result};

/// A page that takes longer than this to resolve is given up on.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);

/// One file to download for a video.
#[derive(Debug, Clone)]
pub struct Stream {
    pub format_id: String,
    pub ext: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub delivery: Delivery,
    /// Expected bytes (bitrate x duration) when the size is not known in advance.
    pub size_hint: Option<u64>,
}

/// How a stream's bytes arrive.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    /// One file at `url`.
    File,
    /// An HLS media playlist at `url`, read by the engine.
    Hls,
    /// DASH pieces as yt-dlp listed them: address and byte range `(start, length)`.
    Dash(Vec<(String, Option<(u64, u64)>)>),
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub title: String,
    /// Seconds, as the site gave it.
    pub duration: Option<f64>,
    /// One stream, or video then audio to be joined.
    pub streams: Vec<Stream>,
}

impl Resolved {
    /// `<title>.<container>`, the container holding every stream unchanged.
    pub fn file_name(&self) -> String {
        let ext = match self.streams.as_slice() {
            [v, a] => media::container(Path::new(&format!("v.{}", v.ext)), Path::new(&format!("a.{}", a.ext))).to_owned(),
            [s, ..] => s.ext.clone(),
            [] => "mp4".to_owned(),
        };
        format!("{}.{ext}", file_stem(&self.title))
    }
}

/// A video title made safe as a file name.
fn file_stem(title: &str) -> String {
    // Long titles would push paths past Windows' limits.
    let title: String = title.replace(['/', '\\'], "_").chars().take(150).collect();
    engine::sanitize_file_name(&title).unwrap_or_else(|| "video".to_owned())
}

#[derive(Deserialize)]
struct Info {
    title: Option<String>,
    id: Option<String>,
    duration: Option<f64>,
    requested_formats: Option<Vec<Format>>,
    #[serde(flatten)]
    single: Format,
}

#[derive(Deserialize)]
struct Format {
    format_id: Option<String>,
    ext: Option<String>,
    url: Option<String>,
    protocol: Option<String>,
    /// Null for some HLS audio tracks.
    http_headers: Option<std::collections::BTreeMap<String, String>>,
    tbr: Option<f64>,
    has_drm: Option<bool>,
    fragment_base_url: Option<String>,
    fragments: Option<Vec<Fragment>>,
}

#[derive(Deserialize)]
struct Fragment {
    url: Option<String>,
    path: Option<String>,
    duration: Option<f64>,
    /// As yt-dlp's own piece downloader reads it: `end` is exclusive.
    byte_range: Option<ByteRange>,
}

#[derive(Deserialize)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl Format {
    fn stream(self, duration: Option<f64>) -> Result<Stream> {
        let id = self.format_id.unwrap_or_default();
        if self.has_drm == Some(true) {
            return Err(engine::Error::Protected(format!("format {id}")).into());
        }
        let protocol = self.protocol.unwrap_or_default();
        let fragments = self.fragments.unwrap_or_default();
        let duration = duration.or_else(|| Some(fragments.iter().filter_map(|f| f.duration).sum::<f64>()).filter(|d| *d > 0.0));
        let delivery = match protocol.as_str() {
            "http" | "https" => Delivery::File,
            "m3u8_native" | "m3u8" => Delivery::Hls,
            "http_dash_segments" => {
                let base = self.fragment_base_url.unwrap_or_default();
                let pieces = fragments
                    .into_iter()
                    .map(|f| {
                        let url = match (f.url, f.path) {
                            (Some(url), _) => url,
                            (None, Some(path)) => format!("{base}{path}"),
                            (None, None) => return Err(Error::Tool(format!("format {id}: a piece without an address"))),
                        };
                        Ok((url, f.byte_range.map(|r| (r.start, r.end.saturating_sub(r.start)))))
                    })
                    .collect::<Result<Vec<_>>>()?;
                if pieces.is_empty() {
                    return Err(Error::Tool(format!("format {id}: yt-dlp listed no DASH pieces")));
                }
                Delivery::Dash(pieces)
            }
            other => return Err(Error::Tool(format!("format {id} is delivered over {other}, which RDM cannot download"))),
        };
        Ok(Stream {
            format_id: id,
            ext: self.ext.unwrap_or_else(|| "mp4".into()),
            url: self.url.ok_or_else(|| Error::Tool("yt-dlp gave no stream address".into()))?,
            headers: self.http_headers.unwrap_or_default().into_iter().collect(),
            size_hint: self.tbr.zip(duration).map(|(kbps, secs)| (kbps * 125.0 * secs) as u64),
            delivery,
        })
    }
}

/// How long a page's listing is kept: shown as the quality list, then used again by Start
/// (or listed in advance while the video plays). Its stream addresses last for hours.
const KEEP: Duration = Duration::from_secs(10 * 60);
/// Pages kept at most.
const KEPT: usize = 16;

/// yt-dlp's listings of pages asked about lately: one run per page, however many ask while
/// it runs. A failed run is not kept.
pub struct Listings {
    keep: Duration,
    pages: Mutex<HashMap<String, Arc<Asked>>>,
}

struct Asked {
    at: Instant,
    json: OnceCell<std::result::Result<Arc<Vec<u8>>, String>>,
}

impl Default for Listings {
    fn default() -> Self {
        Listings::keeping(KEEP)
    }
}

impl Listings {
    fn keeping(keep: Duration) -> Self {
        Listings { keep, pages: Mutex::new(HashMap::new()) }
    }

    /// The listing of `page`: kept, still running, or from a new `run`.
    async fn get<F: Future<Output = Result<Vec<u8>>>>(&self, page: &str, run: impl FnOnce() -> F) -> Result<Arc<Vec<u8>>> {
        let asked = {
            let mut pages = self.pages.lock().unwrap();
            pages.retain(|_, a| a.at.elapsed() < self.keep);
            if !pages.contains_key(page)
                && pages.len() >= KEPT
                && let Some(oldest) = pages.iter().min_by_key(|(_, a)| a.at).map(|(p, _)| p.clone())
            {
                pages.remove(&oldest);
            }
            let fresh = || Arc::new(Asked { at: Instant::now(), json: OnceCell::new() });
            pages.entry(page.to_owned()).or_insert_with(fresh).clone()
        };
        let answer = asked.json.get_or_init(|| async { run().await.map(Arc::new).map_err(|e| e.to_string()) }).await.clone();
        if answer.is_err() {
            let mut pages = self.pages.lock().unwrap();
            if pages.get(page).is_some_and(|a| Arc::ptr_eq(a, &asked)) {
                pages.remove(page);
            }
        }
        answer.map_err(Error::Tool)
    }

    /// A finished listing of `page` that is still kept.
    fn kept(&self, page: &str) -> Option<Arc<Vec<u8>>> {
        let pages = self.pages.lock().unwrap();
        let asked = pages.get(page).filter(|a| a.at.elapsed() < self.keep)?;
        asked.json.get()?.as_ref().ok().cloned()
    }
}

/// The streams of `format` (a yt-dlp format choice) on `page`, which may also be a
/// playlist address. `headers` are the browser's (Referer, Cookie, ...). Plain format ids
/// from a kept listing (the quality list's `299+140`) need no new yt-dlp run.
pub async fn resolve(tools: &Tools, listings: &Listings, page: &str, headers: &[(String, String)], format: &str) -> Result<Resolved> {
    if let Some(json) = listings.kept(page)
        && let Some(resolved) = from_listing(&json, format)?
    {
        return Ok(resolved);
    }
    parse(&yt_dlp(tools, page, headers, &["-f", format]).await?)
}

/// What a page or playlist offers, for the person to choose from (as IDM's list of qualities).
pub async fn choices(tools: &Tools, listings: &Listings, page: &str, headers: &[(String, String)]) -> Result<VideoInfo> {
    video_info(&listings.get(page, || yt_dlp(tools, page, headers, &[])).await?)
}

/// Starts listing `page` in the background, so its quality list is ready (or under way)
/// when asked for.
pub async fn list_ahead(tools: &Tools, listings: &Listings, page: &str, headers: &[(String, String)]) -> Result<()> {
    listings.get(page, || yt_dlp(tools, page, headers, &[])).await.map(drop)
}

/// The streams of `format` taken from a page's listing: plain ids (`299+140`, `18`) only;
/// None for a choice only yt-dlp can make (`bv*+ba/b`) or an id the listing lacks.
pub(crate) fn from_listing(json: &[u8], format: &str) -> Result<Option<Resolved>> {
    #[derive(Deserialize)]
    struct Page {
        title: Option<String>,
        id: Option<String>,
        duration: Option<f64>,
        #[serde(default)]
        formats: Vec<serde_json::Value>,
    }
    let page: Page = serde_json::from_slice(json)?;
    let ids: Vec<&str> = format.split('+').collect();
    if ids.len() > 2 {
        return Ok(None);
    }
    let mut streams = Vec::new();
    for id in ids {
        let Some(offered) = page.formats.iter().find(|f| f["format_id"].as_str() == Some(id)) else { return Ok(None) };
        let format: Format = serde_json::from_value(offered.clone())?;
        streams.push(format.stream(page.duration)?);
    }
    let title = page.title.or(page.id).unwrap_or_else(|| "video".into());
    Ok(Some(Resolved { title, duration: page.duration, streams }))
}

/// yt-dlp's JSON for `page`.
async fn yt_dlp(tools: &Tools, page: &str, headers: &[(String, String)], extra: &[&str]) -> Result<Vec<u8>> {
    let yt_dlp = tools.path(Tool::YtDlp).ok_or_else(|| Error::Tool("yt-dlp is not installed (rdm tools update)".into()))?;
    let mut cmd = media::command(&yt_dlp);
    cmd.args(["--ignore-config", "--no-playlist", "--no-warnings", "-J"]).args(extra);
    cmd.arg("--cache-dir").arg(tools.dir().join("cache"));
    if let Some(deno) = tools.path(Tool::Deno) {
        let mut runtime = std::ffi::OsString::from("deno:");
        runtime.push(deno);
        cmd.arg("--js-runtimes").arg(runtime);
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("user-agent") {
            cmd.arg("--user-agent").arg(value);
        } else {
            // yt-dlp keeps a Cookie header for the address's own domain only.
            cmd.arg("--add-header").arg(format!("{name}:{value}"));
        }
    }
    cmd.arg("--").arg(page);
    let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Tool(format!("yt-dlp did not answer within {} s", RESOLVE_TIMEOUT.as_secs())))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.lines().rev().find(|l| l.starts_with("ERROR:")).unwrap_or(stderr.trim());
        return Err(Error::Tool(format!("yt-dlp: {}", message.trim_start_matches("ERROR:").trim())));
    }
    Ok(output.stdout)
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoInfo {
    pub title: String,
    /// The title as a file name, without extension (each choice has its own).
    pub file_stem: String,
    /// Seconds.
    pub duration: Option<f64>,
    pub choices: Vec<Choice>,
    /// The playlists the lines come from, as yt-dlp names them; empty when a line comes from
    /// none (a file of its own). When the browser's tab played all of them, this list is
    /// those streams once more.
    pub manifests: Vec<String>,
}

/// One line of the quality list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Choice {
    /// yt-dlp format to download: `299+140`, or one id for a single file.
    pub format: String,
    /// Extension of the saved file.
    pub ext: String,
    /// `1920x1080 60fps`, `Audio 129 kbps`; `2100 kbps` or `Video` when a playlist names
    /// no picture size; the container is `ext`.
    pub label: String,
    /// The picture's short side (1080 for 1920x1080, also for upright videos), for
    /// grouping the list; 0 when a playlist names no picture size; None for audio alone.
    pub resolution: Option<u32>,
    /// `H.264 + AAC`; empty when the playlist does not say.
    pub codecs: String,
    /// Bytes.
    pub size: Option<u64>,
    /// The size is an estimate (the site's, or bitrate x duration).
    pub approx: bool,
    /// `HLS` or `DASH` for streams delivered in pieces; None for plain files.
    pub stream: Option<String>,
}

#[derive(Deserialize)]
struct Listing {
    title: Option<String>,
    id: Option<String>,
    duration: Option<f64>,
    #[serde(default)]
    formats: Vec<Offered>,
}

#[derive(Deserialize, Clone)]
struct Offered {
    format_id: String,
    ext: String,
    protocol: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    tbr: Option<f64>,
    abr: Option<f64>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    dynamic_range: Option<String>,
    has_drm: Option<bool>,
    /// The HLS or DASH playlist the format was read from.
    manifest_url: Option<String>,
    /// `English, high` for an HLS audio track.
    format_note: Option<String>,
    /// Higher for the video's original audio track than for dubbed ones.
    language_preference: Option<i32>,
    /// yt-dlp's order among otherwise equal formats (HLS audio tracks).
    source_preference: Option<i32>,
    #[serde(default)]
    fragments: Vec<OfferedPiece>,
    /// A picture, though neither codec nor size is named: set in listings made from a
    /// playlist's text (`caught`), never by yt-dlp.
    #[serde(default)]
    picture: bool,
}

#[derive(Deserialize, Clone)]
struct OfferedPiece {
    duration: Option<f64>,
}

impl Offered {
    fn downloadable(&self) -> bool {
        let known = matches!(self.protocol.as_deref(), Some("http" | "https" | "m3u8_native" | "m3u8" | "http_dash_segments"));
        known && self.ext != "mhtml" && self.has_drm != Some(true)
    }
    /// `HLS` / `DASH` for streams delivered in pieces.
    fn stream(&self) -> Option<&'static str> {
        match self.protocol.as_deref() {
            Some("m3u8_native" | "m3u8") => Some("HLS"),
            Some("http_dash_segments") => Some("DASH"),
            _ => None,
        }
    }
    fn has(codec: &Option<String>) -> bool {
        codec.as_deref().is_some_and(|c| c != "none")
    }
    /// A picture: a known video codec, or an HLS variant that only gives its resolution.
    fn is_video(&self) -> bool {
        self.vcodec.as_deref() != Some("none") && (Self::has(&self.vcodec) || self.height.is_some() || self.picture)
    }
    /// Sound alone. HLS audio tracks often name no codec, only "no video".
    fn is_audio(&self) -> bool {
        self.vcodec.as_deref() == Some("none") && self.acodec.as_deref() != Some("none")
    }
    /// Size in bytes and whether it is an estimate.
    fn size(&self, duration: Option<f64>) -> Option<(u64, bool)> {
        if let Some(exact) = self.filesize {
            return Some((exact, false));
        }
        if let Some(approx) = self.filesize_approx {
            return Some((approx, true));
        }
        let pieces: f64 = self.fragments.iter().filter_map(|f| f.duration).sum();
        let secs = duration.or((pieces > 0.0).then_some(pieces))?;
        self.tbr.map(|kbps| ((kbps * 125.0 * secs) as u64, true))
    }
    /// YouTube's "dynamic range compressed" audio copies (`140-drc`) are quieter
    /// versions of the same track.
    fn drc(&self) -> bool {
        self.format_id.ends_with("-drc")
    }
    fn audio_rank(&self) -> (i32, bool, u64, i32) {
        (
            self.language_preference.unwrap_or(0),
            !self.drc(),
            (self.abr.or(self.tbr).unwrap_or(0.0) * 1000.0) as u64,
            self.source_preference.unwrap_or(0),
        )
    }
}

/// Audio that a video's container holds as it is.
fn fits(video_ext: &str, audio_ext: &str) -> bool {
    match video_ext {
        "webm" => audio_ext == "webm",
        _ => matches!(audio_ext, "m4a" | "mp4"),
    }
}

fn codec_name(codec: &str) -> &str {
    let family = codec.split('.').next().unwrap_or(codec);
    match family {
        "avc1" | "avc3" | "h264" => "H.264",
        "hev1" | "hvc1" | "h265" => "H.265",
        "av01" => "AV1",
        "vp09" | "vp9" => "VP9",
        "vp8" => "VP8",
        "mp4a" | "aac" => "AAC",
        "ac-3" => "AC-3",
        "ec-3" => "E-AC-3",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "mp3" => "MP3",
        other => other,
    }
}

/// `H.264 + AAC`, leaving out what is unknown.
fn codecs(video: Option<&str>, audio: Option<&str>) -> String {
    [video, audio].into_iter().flatten().filter(|c| !c.is_empty() && *c != "none").map(codec_name).collect::<Vec<_>>().join(" + ")
}

/// The quality list from what the page's own player was given: there at once, while
/// yt-dlp is still asked. For showing only: the formats carry the site's numbers, which
/// are yt-dlp's names for most but not all of them (the tracks of a video in several
/// languages share one number), so a line is started from yt-dlp's list.
pub fn from_player(player: &crate::model::PlayerData) -> Result<VideoInfo> {
    if player.live {
        return Err(engine::Error::Live.into());
    }
    let formats: Vec<serde_json::Value> = player
        .formats
        .iter()
        .filter(|f| !f.drm)
        .filter_map(|f| {
            // `video/mp4; codecs="avc1.42001E, mp4a.40.2"`: the picture's codec first.
            let (kind, container) = f.mime.split(';').next()?.trim().split_once('/')?;
            let codecs = f.mime.split_once("codecs=").map_or("", |(_, c)| c.trim().trim_matches('"'));
            let mut codecs = codecs.split(',').map(str::trim).filter(|c| !c.is_empty());
            let (picture, sound, ext) = match (kind, container) {
                ("video", "mp4" | "webm") => (codecs.next(), codecs.next(), container),
                ("audio", "mp4") => (None, codecs.next(), "m4a"),
                ("audio", "webm") => (None, codecs.next(), container),
                _ => return None,
            };
            let kbps = f.bitrate.map(|b| b / 1000.0);
            Some(serde_json::json!({
                "format_id": format!("{}{}", f.itag, if f.drc { "-drc" } else { "" }),
                "ext": ext,
                "protocol": "https",
                "vcodec": picture.unwrap_or("none"),
                "acodec": sound.unwrap_or("none"),
                "width": f.width,
                "height": f.height,
                "fps": f.fps,
                "tbr": kbps,
                "abr": kbps.filter(|_| kind == "audio"),
                "filesize": f.size,
                "dynamic_range": f.quality.as_deref().filter(|q| q.contains("HDR")).map(|_| "HDR10"),
                "format_note": f.track,
                // As yt-dlp ranks the tracks of a video in several languages.
                "language_preference": f.track.as_ref().map(|_| match (f.original, f.preferred) {
                    (true, _) => 10,
                    (false, true) => 5,
                    (false, false) => -1,
                }),
            }))
        })
        .collect();
    video_info(&serde_json::to_vec(&serde_json::json!({ "id": player.id, "title": player.title, "duration": player.seconds, "formats": formats }))?)
}

pub(crate) fn video_info(json: &[u8]) -> Result<VideoInfo> {
    let listing: Listing = serde_json::from_slice(json)?;
    // A DASH manifest's length may only be known from its pieces.
    let duration = listing.duration.or_else(|| {
        let longest = listing.formats.iter().map(|f| f.fragments.iter().filter_map(|p| p.duration).sum::<f64>()).fold(0.0, f64::max);
        (longest > 0.0).then_some(longest)
    });
    let offered: Vec<&Offered> = listing.formats.iter().filter(|f| f.downloadable()).collect();
    let best_audio = |fits: &dyn Fn(&str) -> bool| {
        offered.iter().filter(|f| f.is_audio() && fits(&f.ext)).max_by_key(|f| f.audio_rank()).copied()
    };

    let mut choices: Vec<(Vec<u64>, Choice)> = Vec::new();
    for v in offered.iter().filter(|f| f.is_video()) {
        let dims = v.width.zip(v.height);
        if dims.is_none() && !v.picture {
            continue;
        }
        let fps = v.fps.map(|f| f.round() as u32).filter(|&f| f > 0);
        let hdr = v.dynamic_range.as_deref().is_some_and(|r| r != "SDR");
        let vname = codec_name(v.vcodec.as_deref().unwrap_or(""));
        // A video without sound needs an audio track; an unknown audio codec counts as sound
        // (HLS variants without a CODECS list carry their own).
        let (format, ext, codecs, size) = if v.acodec.as_deref() != Some("none") {
            (v.format_id.clone(), v.ext.clone(), codecs(v.vcodec.as_deref(), v.acodec.as_deref()), v.size(duration))
        } else {
            // Sound its container holds as it is, else any (the pair then goes to Matroska).
            let Some(a) = best_audio(&|ext| fits(&v.ext, ext)).or_else(|| best_audio(&|_| true)) else { continue };
            let ext = media::container(Path::new(&format!("v.{}", v.ext)), Path::new(&format!("a.{}", a.ext))).to_owned();
            let size = v.size(duration).zip(a.size(duration)).map(|((x, xa), (y, ya))| (x + y, xa || ya));
            (format!("{}+{}", v.format_id, a.format_id), ext, codecs(v.vcodec.as_deref(), a.acodec.as_deref()), size)
        };
        let named = match (dims, v.tbr) {
            (Some((w, h)), _) => format!("{w}x{h}"),
            (None, Some(kbps)) => format!("{} kbps", kbps.round()),
            (None, None) => "Video".to_owned(),
        };
        let label = format!("{named}{}{}", fps.map(|f| format!(" {f}fps")).unwrap_or_default(), if hdr { " HDR" } else { "" });
        // Highest resolution first, then frame rate; then a fixed order of containers (MP4,
        // WebM, MKV) and codecs, so equal resolutions always list the same way; a plain
        // file before a playlist that looks the same.
        let container_rank = match ext.as_str() {
            "mp4" => 3,
            "webm" => 2,
            _ => 1,
        };
        let codec_rank = match vname {
            "H.264" => 4,
            "H.265" => 3,
            "VP9" => 2,
            "AV1" => 1,
            _ => 0,
        };
        let resolution = dims.map_or(0, |(w, h)| w.min(h));
        let key = vec![
            resolution as u64,
            fps.unwrap_or(0) as u64,
            hdr as u64,
            container_rank,
            codec_rank,
            v.stream().is_none() as u64,
            (v.tbr.unwrap_or(0.0) * 1000.0) as u64,
        ];
        let stream = v.stream().map(str::to_owned);
        let (size, approx) = size.map_or((None, false), |(s, a)| (Some(s), a));
        choices.push((key, Choice { format, ext, label, resolution: Some(resolution), codecs, size, approx, stream }));
    }
    choices.sort_by(|a, b| b.0.cmp(&a.0));
    // One line per container, label and codecs: the best of equal-looking formats.
    let mut seen = std::collections::HashSet::new();
    let mut list: Vec<Choice> =
        choices.into_iter().map(|(_, c)| c).filter(|c| seen.insert((c.ext.clone(), c.label.clone(), c.codecs.clone()))).collect();

    let families: [fn(&str) -> bool; 2] = [|e| matches!(e, "m4a" | "mp4"), |e| e == "webm"];
    for family in families {
        if let Some(a) = best_audio(&family) {
            let kbps = a.abr.or(a.tbr).map(|k| format!(" {} kbps", k.round()));
            let note = a.format_note.as_deref().filter(|_| kbps.is_none()).map(|n| format!(" ({n})"));
            // MP4 audio alone is saved as .m4a.
            let ext = if a.ext == "mp4" { "m4a".to_owned() } else { a.ext.clone() };
            let (size, approx) = a.size(duration).map_or((None, false), |(s, x)| (Some(s), x));
            list.push(Choice {
                format: a.format_id.clone(),
                ext,
                label: format!("Audio{}", kbps.or(note).unwrap_or_default()),
                resolution: None,
                codecs: codecs(None, a.acodec.as_deref()),
                size,
                approx,
                stream: a.stream().map(str::to_owned),
            });
        }
    }
    if list.is_empty() {
        let protected = listing.formats.iter().any(|f| f.has_drm == Some(true));
        return Err(Error::Tool(if protected {
            "the video is DRM-protected, which RDM cannot download".into()
        } else {
            "no downloadable video here".into()
        }));
    }
    let from: Vec<Option<&String>> = list
        .iter()
        .flat_map(|c| c.format.split('+'))
        .map(|id| offered.iter().find(|f| f.format_id == id).and_then(|f| f.manifest_url.as_ref()))
        .collect();
    let mut manifests: Vec<String> = Vec::new();
    for url in from.iter().flatten().filter(|_| from.iter().all(Option::is_some)) {
        if !manifests.contains(url) {
            manifests.push((*url).clone());
        }
    }
    let title = listing.title.or(listing.id).unwrap_or_else(|| "video".into());
    Ok(VideoInfo { file_stem: file_stem(&title), title, duration, choices: list, manifests })
}

fn parse(json: &[u8]) -> Result<Resolved> {
    let info: Info = serde_json::from_slice(json)?;
    let title = info.title.or(info.id).unwrap_or_else(|| "video".into());
    let streams = match info.requested_formats {
        Some(formats) => formats.into_iter().map(|f| f.stream(info.duration)).collect::<Result<Vec<_>>>()?,
        None => vec![info.single.stream(info.duration)?],
    };
    if streams.len() > 2 {
        return Err(Error::Tool(format!("yt-dlp chose {} streams; one or two are supported", streams.len())));
    }
    Ok(Resolved { title, duration: info.duration, streams })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing as `yt-dlp -J` prints it, with stream addresses.
    const LISTED: &[u8] = br#"{"id":"abc","title":"Clip","duration":30.5,"formats":[
        {"format_id":"140","ext":"m4a","url":"https://r1.googlevideo.com/videoplayback?itag=140","protocol":"https","http_headers":{"User-Agent":"UA"}},
        {"format_id":"299","ext":"mp4","url":"https://r1.googlevideo.com/videoplayback?itag=299","protocol":"https","tbr":3000.0,"http_headers":{}},
        {"format_id":"18","ext":"mp4","url":"https://r1.googlevideo.com/videoplayback?itag=18","protocol":"https"},
        {"format_id":"drm","ext":"mp4","url":"https://x/drm","protocol":"https","has_drm":true}
    ]}"#;

    #[test]
    fn plain_ids_come_from_a_kept_listing() {
        let r = from_listing(LISTED, "299+140").unwrap().unwrap();
        assert_eq!((r.title.as_str(), r.duration), ("Clip", Some(30.5)));
        let ids: Vec<(&str, &str)> = r.streams.iter().map(|s| (s.format_id.as_str(), s.url.as_str())).collect();
        assert_eq!(ids, [("299", "https://r1.googlevideo.com/videoplayback?itag=299"), ("140", "https://r1.googlevideo.com/videoplayback?itag=140")]);
        assert_eq!(r.streams[1].headers, [("User-Agent".to_owned(), "UA".to_owned())]);
        assert_eq!(r.streams[0].size_hint, Some((3000.0 * 125.0 * 30.5) as u64));
        assert_eq!(from_listing(LISTED, "18").unwrap().unwrap().streams.len(), 1);
        // Only yt-dlp can make these choices.
        for format in ["bv*+ba/b", "best", "999", "299+999", "18+140+299"] {
            assert!(from_listing(LISTED, format).unwrap().is_none(), "{format}");
        }
        assert!(matches!(from_listing(LISTED, "drm"), Err(Error::Download(engine::Error::Protected(_)))));
    }

    #[tokio::test]
    async fn one_run_per_page_kept_for_a_while() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Kept long enough that a slow machine does not lose the answer between the checks.
        let listings = Listings::keeping(Duration::from_secs(2));
        let runs = AtomicUsize::new(0);
        let run = || async {
            runs.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(b"{}".to_vec())
        };
        // Two asking at once share one run; asking again later gets the kept answer.
        let (a, b) = tokio::join!(listings.get("p", run), listings.get("p", run));
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        listings.get("p", run).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(listings.kept("p").is_some() && listings.kept("other").is_none());
        // Past its time it runs again.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert!(listings.kept("p").is_none());
        listings.get("p", run).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // A failure is not kept.
        let failing = || async { Err(Error::Tool("yt-dlp: no".into())) };
        assert!(listings.get("q", failing).await.is_err());
        assert!(listings.kept("q").is_none());
        listings.get("q", run).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn video_and_audio_pair() {
        let json = br#"{"id":"aqz","title":"Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film",
            "ext":"mp4","requested_formats":[
            {"format_id":"299","ext":"mp4","url":"https://r1.googlevideo.com/videoplayback?itag=299","protocol":"https",
             "http_headers":{"User-Agent":"Mozilla/5.0"}},
            {"format_id":"140","ext":"m4a","url":"https://r1.googlevideo.com/videoplayback?itag=140","protocol":"https",
             "http_headers":{}}]}"#;
        let r = parse(json).unwrap();
        assert_eq!(r.streams.len(), 2);
        assert_eq!((r.streams[0].format_id.as_str(), r.streams[1].ext.as_str()), ("299", "m4a"));
        assert_eq!(r.streams[0].headers, [("User-Agent".to_string(), "Mozilla/5.0".to_string())]);
        assert_eq!(r.streams[0].delivery, Delivery::File);
        assert_eq!(r.file_name(), "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film.mp4");
    }

    #[test]
    fn single_file_and_unsafe_title() {
        let json = br#"{"id":"x","title":"AC/DC: Live?","format_id":"18","ext":"mp4","url":"https://h/v.mp4","protocol":"https"}"#;
        let r = parse(json).unwrap();
        assert_eq!(r.streams.len(), 1);
        assert_eq!(r.file_name(), "AC_DC_ Live_.mp4");
    }

    #[test]
    fn mixed_pair_goes_to_matroska() {
        let json = br#"{"title":"t","requested_formats":[
            {"format_id":"401","ext":"mp4","url":"https://h/v","protocol":"https"},
            {"format_id":"251","ext":"webm","url":"https://h/a","protocol":"https"}]}"#;
        assert_eq!(parse(json).unwrap().file_name(), "t.mkv");
    }

    #[test]
    fn playlist_streams_resolve_to_pieces() {
        // Shaped like yt-dlp's answer for Apple's HLS example and Akamai's DASH example.
        let json = br#"{"title":"master","duration":600,"requested_formats":[
            {"format_id":"8190","ext":"mp4","protocol":"m3u8_native","tbr":8190.9,
             "url":"https://cdn/v9/prog_index.m3u8","http_headers":{"Referer":"https://site/"}},
            {"format_id":"bbb_a64k","ext":"m4a","protocol":"http_dash_segments","url":"https://cdn/bbb.mpd",
             "fragment_base_url":"https://cdn/bbb/./","fragments":[{"path":"a_0.m4a"},
             {"path":"a_1.m4a","duration":4.0},{"url":"https://other/a_2.m4a","duration":4.0,"byte_range":{"start":10,"end":30}}]}]}"#;
        let r = parse(json).unwrap();
        assert_eq!((r.streams[0].delivery.clone(), r.streams[0].size_hint), (Delivery::Hls, Some(614_317_500)));
        let Delivery::Dash(pieces) = &r.streams[1].delivery else { panic!("{:?}", r.streams[1].delivery) };
        assert_eq!(
            pieces,
            &[
                ("https://cdn/bbb/./a_0.m4a".to_owned(), None),
                ("https://cdn/bbb/./a_1.m4a".to_owned(), None),
                ("https://other/a_2.m4a".to_owned(), Some((10, 20)))
            ]
        );
        assert_eq!(r.file_name(), "master.mp4");
    }

    #[test]
    fn hls_audio_track_without_headers() {
        let json = br#"{"title":"t","format_id":"aud2-English","ext":"mp4","protocol":"m3u8_native",
            "url":"https://cdn/a2/prog_index.m3u8","http_headers":null}"#;
        assert!(parse(json).unwrap().streams[0].headers.is_empty());
    }

    #[test]
    fn other_protocols_and_drm_are_refused() {
        let json = br#"{"title":"t","format_id":"f4m-1","ext":"flv","url":"https://h/m.f4m","protocol":"f4m"}"#;
        assert!(parse(json).unwrap_err().to_string().contains("f4m"));
        let json = br#"{"title":"t","format_id":"d","ext":"mp4","url":"https://h/m.mpd","protocol":"http_dash_segments","has_drm":true}"#;
        assert!(parse(json).unwrap_err().to_string().contains("DRM"));
    }

    /// Trimmed from yt-dlp's answer for a YouTube video (addresses left out).
    const LISTING: &[u8] = br#"{"id":"aqz","title":"Bunny","duration":634.5,"formats":[
        {"format_id":"sb0","ext":"mhtml","protocol":"mhtml","vcodec":"none","acodec":"none"},
        {"format_id":"140-drc","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":129.5,"filesize":10276000,"language_preference":-1},
        {"format_id":"140","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":129.5,"filesize":10276317,"language_preference":-1},
        {"format_id":"251","ext":"webm","protocol":"https","vcodec":"none","acodec":"opus","abr":129.4,"filesize":10204000,"language_preference":-1},
        {"format_id":"160","ext":"mp4","protocol":"https","vcodec":"avc1.4D400C","acodec":"none","width":256,"height":144,"fps":30,"tbr":55,"filesize":4323893},
        {"format_id":"394","ext":"mp4","protocol":"https","vcodec":"av01.0.00M.08","acodec":"none","width":256,"height":144,"fps":30,"tbr":58,"filesize":4630000},
        {"format_id":"299","ext":"mp4","protocol":"https","vcodec":"avc1.64002A","acodec":"none","width":1920,"height":1080,"fps":60,"tbr":3248,"filesize":257619653},
        {"format_id":"312","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.64002A","acodec":"none","width":1920,"height":1080,"fps":60,"tbr":7418},
        {"format_id":"303","ext":"webm","protocol":"https","vcodec":"vp9","acodec":"none","width":1920,"height":1080,"fps":60,"tbr":2127,"filesize_approx":168736189},
        {"format_id":"401","ext":"mp4","protocol":"https","vcodec":"av01.0.13M.08","acodec":"none","width":3840,"height":2160,"fps":60,"tbr":8982,"dynamic_range":"HDR10","filesize":712400000},
        {"format_id":"18","ext":"mp4","protocol":"https","vcodec":"avc1.42001E","acodec":"mp4a.40.2","width":640,"height":360,"fps":30,"tbr":500,"filesize":40000000}
    ]}"#;

    #[test]
    fn choices_pair_streams_by_container_and_list_audio_last() {
        let info = video_info(LISTING).unwrap();
        assert_eq!((info.title.as_str(), info.duration), ("Bunny", Some(634.5)));
        let lines: Vec<(&str, &str, &str, &str)> =
            info.choices.iter().map(|c| (c.format.as_str(), c.label.as_str(), c.codecs.as_str(), c.ext.as_str())).collect();
        // 312 (HLS) looks the same as 299 and loses to the plain file.
        assert_eq!(
            lines,
            [
                ("401+140", "3840x2160 60fps HDR", "AV1 + AAC", "mp4"),
                ("299+140", "1920x1080 60fps", "H.264 + AAC", "mp4"),
                ("303+251", "1920x1080 60fps", "VP9 + Opus", "webm"),
                ("18", "640x360 30fps", "H.264 + AAC", "mp4"),
                ("160+140", "256x144 30fps", "H.264 + AAC", "mp4"),
                ("394+140", "256x144 30fps", "AV1 + AAC", "mp4"),
                ("140", "Audio 130 kbps", "AAC", "m4a"),
                ("251", "Audio 129 kbps", "Opus", "webm"),
            ]
        );
        let resolutions: Vec<Option<u32>> = info.choices.iter().map(|c| c.resolution).collect();
        assert_eq!(resolutions, [Some(2160), Some(1080), Some(1080), Some(360), Some(144), Some(144), None, None]);
        // Video and audio sizes add up; an estimate makes the sum an estimate.
        assert_eq!((info.choices[1].size, info.choices[1].approx), (Some(257619653 + 10276317), false));
        assert_eq!((info.choices[2].size, info.choices[2].approx), (Some(168736189 + 10204000), true));
        assert!(info.choices.iter().all(|c| c.stream.is_none()));
    }

    /// A format yt-dlp lists with a video codec but no picture size stays out, as before
    /// playlists were listed from their text (those mark their own such formats).
    #[test]
    fn a_page_format_without_a_picture_size_is_not_listed() {
        let json = br#"{"title":"t","formats":[
            {"format_id":"sized","ext":"mp4","protocol":"https","vcodec":"avc1.64002A","acodec":"mp4a.40.2","width":640,"height":360},
            {"format_id":"bare","ext":"mp4","protocol":"https","vcodec":"avc1.64002A","acodec":"mp4a.40.2","tbr":900}]}"#;
        let formats: Vec<String> = video_info(json).unwrap().choices.into_iter().map(|c| c.format).collect();
        assert_eq!(formats, ["sized"]);
    }

    #[test]
    fn original_audio_is_paired_over_a_dub() {
        let json = br#"{"title":"t","formats":[
            {"format_id":"140-1","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":129,"language_preference":-1},
            {"format_id":"140-0","ext":"m4a","protocol":"https","vcodec":"none","acodec":"mp4a.40.2","abr":128,"language_preference":10},
            {"format_id":"299","ext":"mp4","protocol":"https","vcodec":"avc1.64002A","acodec":"none","width":1920,"height":1080,"fps":60}]}"#;
        assert_eq!(video_info(json).unwrap().choices[0].format, "299+140-0");
    }

    /// Shaped like yt-dlp's answer for Apple's HLS example: video-only variants and audio
    /// tracks without a codec.
    #[test]
    fn hls_variants_pair_with_audio_tracks() {
        let json = br#"{"title":"master","duration":600,"formats":[
            {"format_id":"aud1-English","ext":"mp4","protocol":"m3u8_native","vcodec":"none","format_note":"English, low","source_preference":0},
            {"format_id":"aud2-English","ext":"mp4","protocol":"m3u8_native","vcodec":"none","format_note":"English, high","source_preference":2},
            {"format_id":"7968","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.64002a","acodec":"none","width":1920,"height":1080,"fps":60,"tbr":7968.4},
            {"format_id":"8190","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.64002a","acodec":"none","width":1920,"height":1080,"fps":60,"tbr":8190.9},
            {"format_id":"530","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.640015","acodec":"none","width":480,"height":270,"fps":30,"tbr":530.7}]}"#;
        let info = video_info(json).unwrap();
        let lines: Vec<(&str, &str, &str, Option<&str>)> =
            info.choices.iter().map(|c| (c.format.as_str(), c.label.as_str(), c.codecs.as_str(), c.stream.as_deref())).collect();
        assert_eq!(
            lines,
            [
                ("8190+aud2-English", "1920x1080 60fps", "H.264", Some("HLS")),
                ("530+aud2-English", "480x270 30fps", "H.264", Some("HLS")),
                ("aud2-English", "Audio (English, high)", "", Some("HLS")),
            ]
        );
        assert_eq!(info.choices[2].ext, "m4a");
        // 8190 kbps for 600 s, the audio track's size unknown: no sum.
        assert_eq!(info.choices[0].size, None);
    }

    #[test]
    fn hls_variants_with_their_own_sound_and_estimated_sizes() {
        let json = br#"{"title":"x36xhzz","duration":634,"formats":[
            {"format_id":"240","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.42000d","acodec":"mp4a.40.5","width":320,"height":184,"tbr":246.44},
            {"format_id":"1080","ext":"mp4","protocol":"m3u8_native","vcodec":"avc1.640028","acodec":"mp4a.40.2","width":1920,"height":1080,"tbr":6221.6}]}"#;
        let info = video_info(json).unwrap();
        let c = &info.choices[0];
        assert_eq!((c.format.as_str(), c.codecs.as_str(), c.size, c.approx), ("1080", "H.264 + AAC", Some(493_061_800), true));
        assert_eq!(info.choices.len(), 2, "no audio-only line when there is no audio-only format");
    }

    #[test]
    fn the_playlists_a_listing_comes_from() {
        let json = br#"{"title":"t","duration":10,"formats":[
            {"format_id":"a","ext":"mp4","protocol":"m3u8_native","height":360,"width":640,"manifest_url":"https://cdn.example/master.m3u8"},
            {"format_id":"b","ext":"mp4","protocol":"m3u8_native","height":720,"width":1280,"manifest_url":"https://cdn.example/master.m3u8"},
            {"format_id":"c","ext":"mp4","protocol":"http_dash_segments","height":720,"width":1280,"acodec":"mp4a","manifest_url":"https://cdn.example/all.mpd"},
            {"format_id":"d","ext":"mp4","protocol":"https","height":240,"width":426},
            {"format_id":"e","ext":"mp4","protocol":"https","height":1080,"width":1920,"has_drm":true,"manifest_url":"https://cdn.example/drm.mpd"}]}"#;
        // A line that is a file of its own (d): the list is more than its playlists.
        assert!(video_info(json).unwrap().manifests.is_empty());
        // Without it: each playlist once; not the playlist of a format that is not offered.
        let json = String::from_utf8_lossy(json).replace(r#"{"format_id":"d","ext":"mp4","protocol":"https","height":240,"width":426},"#, "");
        let mut manifests = video_info(json.as_bytes()).unwrap().manifests;
        manifests.sort();
        assert_eq!(manifests, ["https://cdn.example/all.mpd", "https://cdn.example/master.m3u8"]);
    }

    #[test]
    fn the_list_a_pages_own_player_was_given() {
        use crate::model::PlayerFormat;
        let format = |itag: u32, mime: &str, height: Option<u32>, bitrate: f64, size: Option<u64>| PlayerFormat {
            itag,
            mime: mime.into(),
            width: height.map(|h| h * 16 / 9),
            height,
            fps: height.map(|_| 60.0),
            bitrate: Some(bitrate),
            size,
            ..PlayerFormat::default()
        };
        let mut player = crate::model::PlayerData {
            id: "abc".into(),
            title: Some("A clip: part 1".into()),
            seconds: Some(100.0),
            live: false,
            formats: vec![
                format(18, r#"video/mp4; codecs="avc1.42001E, mp4a.40.2""#, Some(360), 500_000.0, None),
                format(299, r#"video/mp4; codecs="avc1.64002a""#, Some(1080), 4_000_000.0, Some(50_000_000)),
                PlayerFormat { quality: Some("1080p60 HDR".into()), ..format(303, r#"video/webm; codecs="vp09.00.41.08""#, Some(1080), 3_000_000.0, Some(40_000_000)) },
                format(140, r#"audio/mp4; codecs="mp4a.40.2""#, None, 128_000.0, Some(1_600_000)),
                PlayerFormat { drc: true, ..format(140, r#"audio/mp4; codecs="mp4a.40.2""#, None, 128_000.0, Some(1_600_000)) },
                // A video in several languages: the tracks share the number.
                PlayerFormat { track: Some("German".into()), preferred: true, ..format(140, r#"audio/mp4; codecs="mp4a.40.2""#, None, 192_000.0, Some(2_400_000)) },
                PlayerFormat { track: Some("English original".into()), original: true, ..format(140, r#"audio/mp4; codecs="mp4a.40.2""#, None, 128_000.0, Some(1_600_001)) },
                format(251, r#"audio/webm; codecs="opus""#, None, 160_000.0, Some(2_000_000)),
                PlayerFormat { drm: true, ..format(999, r#"video/mp4; codecs="avc1.640033""#, Some(2160), 9_000_000.0, None) },
                format(7, "text/vtt", None, 1.0, None),
                // A type the page made up is no file extension.
                format(8, r#"video/exe; codecs="avc1.640033""#, Some(4320), 9_000_000.0, None),
                format(9, r#"audio/../x; codecs="opus""#, None, 900_000.0, None),
            ],
        };
        let info = from_player(&player).unwrap();
        assert_eq!((info.title.as_str(), info.file_stem.as_str(), info.duration), ("A clip: part 1", "A clip_ part 1", Some(100.0)));
        let lines: Vec<_> = info.choices.iter().map(|c| (c.format.as_str(), c.ext.as_str(), c.label.as_str(), c.codecs.as_str(), c.size, c.approx)).collect();
        assert_eq!(
            lines,
            [
                // The original track, though the viewer's own has more bits; exact sizes
                // added up.
                ("303+251", "webm", "1920x1080 60fps HDR", "VP9 + Opus", Some(42_000_000), false),
                ("299+140", "mp4", "1920x1080 60fps", "H.264 + AAC", Some(51_600_001), false),
                // No size in the page: bit rate x length.
                ("18", "mp4", "640x360 60fps", "H.264 + AAC", Some(6_250_000), true),
                ("140", "m4a", "Audio 128 kbps", "AAC", Some(1_600_001), false),
                ("251", "webm", "Audio 160 kbps", "Opus", Some(2_000_000), false),
            ]
        );
        // Without the original track the viewer's own comes first; and a quieter copy is
        // named as yt-dlp names it.
        player.formats.retain(|f| !f.original);
        let mp4 = |player: &crate::model::PlayerData| from_player(player).unwrap().choices.into_iter().find(|c| c.ext == "mp4").unwrap();
        assert_eq!(mp4(&player).size, Some(52_400_000));
        player.formats.retain(|f| f.itag != 140 || f.drc);
        assert_eq!(mp4(&player).format, "299+140-drc");
        player.live = true;
        assert!(from_player(&player).is_err());
    }

    #[test]
    fn a_picture_whose_container_takes_none_of_the_sound_is_still_offered() {
        let json = br#"{"title":"t","duration":10,"formats":[
            {"format_id":"v","ext":"mp4","protocol":"https","vcodec":"avc1.64001f","acodec":"none","height":720,"width":1280},
            {"format_id":"a","ext":"webm","protocol":"https","vcodec":"none","acodec":"opus","abr":160}]}"#;
        let info = video_info(json).unwrap();
        assert_eq!((info.choices[0].format.as_str(), info.choices[0].ext.as_str(), info.choices[0].codecs.as_str()), ("v+a", "mkv", "H.264 + Opus"));
    }

    #[test]
    fn dash_length_from_its_pieces() {
        let json = br#"{"title":"bbb","formats":[
            {"format_id":"a","ext":"m4a","protocol":"http_dash_segments","vcodec":"none","acodec":"mp4a.40.5","tbr":64,
             "fragments":[{"path":"a0"},{"path":"a1","duration":4.0},{"path":"a2","duration":2.5}]}]}"#;
        let info = video_info(json).unwrap();
        assert_eq!(info.duration, Some(6.5));
        assert_eq!((info.choices[0].size, info.choices[0].approx), (Some(52_000), true));
    }

    #[test]
    fn nothing_downloadable_says_why() {
        let json = br#"{"title":"t","formats":[{"format_id":"f","ext":"flv","protocol":"f4m","vcodec":"h264","width":1,"height":1}]}"#;
        assert!(video_info(json).unwrap_err().to_string().contains("no downloadable video"));
        let json = br#"{"title":"t","formats":[{"format_id":"d","ext":"mp4","protocol":"http_dash_segments","has_drm":true,"vcodec":"avc1","width":1,"height":1}]}"#;
        assert!(video_info(json).unwrap_err().to_string().contains("DRM"));
    }
}
