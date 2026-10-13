//! Multi-connection download into a preallocated `.part` file, with dynamic segmentation,
//! per-connection retries and a resume file that is always consistent with the data on disk.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_RANGE, HeaderMap, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};
use tokio::task::{JoinSet, spawn_blocking};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::probe::{Probe, Validator, parse_content_range, probe};
use crate::segments::{Claim, Segment, Table};
use crate::checksum::hash_file;
use crate::{Error, Limiter, MAX_RETRIES, Result, client};

/// A connection that delivers nothing for this long is dropped and retried.
pub(crate) const STALL: Duration = Duration::from_secs(30);
/// Bytes buffered per connection before a write. Splits never land closer than this to
/// a segment's written position, so a write in flight never crosses a new segment end
/// (a take-over of a stalled or slow connection's rest does; see `Table::take_over`).
const FLUSH: u64 = 256 * 1024;
/// Buffered bytes are also written once they are this old, so a slow connection shows
/// progress and is not taken for stalled (`TAKE_OVER_AFTER`) while bytes keep coming.
const FLUSH_AFTER: Duration = Duration::from_secs(1);
/// A connection's speed is measured over at least this long, so a short segment finished
/// in a burst does not make it look faster than it is.
const SPEED_OVER: Duration = Duration::from_secs(1);
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// Connections opened before the server has answered any (`grow`).
const FIRST_CONNECTIONS: usize = 4;
/// How often a connection with nothing to do asks again (`Claim::Wait`).
const WAIT: Duration = Duration::from_millis(250);
/// Bytes on disk fetched again before a resume, to see that the file is still the same.
const OVERLAP: u64 = 64 * 1024;

#[derive(Clone)]
pub struct Options {
    pub connections: usize,
    pub dir: PathBuf,
    /// Overrides the name learned from the server.
    pub file_name: Option<String>,
    /// Smallest piece a dynamic split may leave on either side (at least 256 KiB).
    pub min_split: u64,
    /// Request headers such as Referer, Cookie and User-Agent.
    pub headers: Vec<(String, String)>,
    /// Every received byte passes all of these, for example a global and a per-download limit.
    pub limiters: Vec<Arc<Limiter>>,
    /// Largest range one request asks for; a segment is then read as a series of requests.
    /// `None` uses `request_cap` for the host (unlimited for most).
    pub max_request: Option<u64>,
    /// The address is new for a download started under another one (an expired link given
    /// again): what is on disk counts when the size matches, even with another ETag or
    /// date (another server's copy); the bytes at the resume point must still match.
    pub new_address: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            connections: 8,
            dir: PathBuf::from("."),
            file_name: None,
            min_split: 1 << 20,
            headers: Vec::new(),
            limiters: Vec::new(),
            max_request: None,
            new_address: false,
        }
    }
}

/// Request size cap known for a host. YouTube's video servers answer 403 to ranges wider
/// than about 20 MiB and throttle open-ended ones; yt-dlp and IDM ask for 10 MiB pieces.
fn request_cap(url: &Url) -> Option<u64> {
    let host = url.host_str()?;
    (host == "googlevideo.com" || host.ends_with(".googlevideo.com")).then_some(10 << 20)
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// `total` is a guess (a playlist without byte ranges, from the pieces so far).
    pub estimated: bool,
    pub connections: usize,
    /// Current split of the file, for a segment bar.
    pub segments: Vec<Segment>,
    /// A playlist download: `(written, all)` pieces.
    pub pieces: Option<(usize, usize)>,
}

#[derive(Debug)]
pub struct Outcome {
    pub path: PathBuf,
    pub size: u64,
    /// Bytes already on disk from an earlier run.
    pub resumed: u64,
    /// Segments the file ended up split into.
    pub segments: usize,
    /// False when the server had no range support and one connection was used.
    pub ranges: bool,
    /// Connections that stopped because the server turned them away (429 / 503).
    pub refused: usize,
    /// What the file was checked against ("SHA-256 from the server (Repr-Digest)").
    pub checked: Option<String>,
}

/// Saved next to the `.part` file; describes exactly which bytes of it are valid.
#[derive(Serialize, Deserialize)]
struct SavedState {
    url: String,
    size: u64,
    validator: Validator,
    segments: Vec<Segment>,
}

struct Shared {
    table: Mutex<Table>,
    file: Arc<File>,
    url: Url,
    headers: HeaderMap,
    validator: Validator,
    ranges: bool,
    /// The probed size; with ranges every answer must report it.
    size: Option<u64>,
    /// Cancelled when every byte is written, so connections still waiting on a server that
    /// lost its segment to a take-over end at once.
    done: CancellationToken,
    limiters: Vec<Arc<Limiter>>,
    /// Largest range per request (u64::MAX for no cap).
    cap: u64,
    /// Requests the server answered with data, and whether it ever refused one (`grow`).
    accepted: AtomicUsize,
    limited: AtomicBool,
    /// Woken on each answered request, so the next connections open at once.
    ramp: Notify,
}

/// Probes `url` and downloads it. Cancelling `stop` saves the state and returns
/// `Error::Cancelled`; the same call later resumes.
pub async fn download(
    url: &str,
    opts: &Options,
    progress: &watch::Sender<Progress>,
    stop: &CancellationToken,
) -> Result<Outcome> {
    let info = tokio::select! {
        info = probe(url, &opts.headers) => info?,
        _ = stop.cancelled() => return Err(Error::Cancelled),
    };
    download_probed(info, opts, progress, stop).await
}

/// Downloads what an earlier `probe` found; `opts.headers` is not used again.
pub async fn download_probed(
    info: Probe,
    opts: &Options,
    progress: &watch::Sender<Progress>,
    stop: &CancellationToken,
) -> Result<Outcome> {
    assert!(opts.min_split >= FLUSH, "min_split must be at least {FLUSH}");
    // One client per connection: a shared client would multiplex HTTP/2 requests over a
    // single TCP connection, and servers throttle per connection.
    let mut clients = (0..opts.connections.max(1)).map(|_| client()).collect::<Result<Vec<_>>>()?;
    let name = opts.file_name.clone().unwrap_or_else(|| info.file_name.clone());
    std::fs::create_dir_all(&opts.dir)?;
    let part = opts.dir.join(format!("{name}.part"));
    let state_path = opts.dir.join(format!("{name}.part.json"));
    let ranges = info.ranges && info.size.is_some_and(|s| s > 0);

    let resumed = if ranges { load_state(&state_path, &part, info.size, &info.validator, opts.new_address) } else { None };
    let resumed = match resumed {
        Some(segments) => tokio::select! {
            same = still_same(&clients[0], &info, &part, &segments) => same?.then_some(segments),
            _ = stop.cancelled() => return Err(Error::Cancelled),
        },
        None => None,
    };
    if resumed.is_none() {
        remove_if_exists(&state_path)?;
    }
    let (file, table) = match resumed {
        Some(segments) => (File::options().write(true).open(&part)?, Table::new(segments, opts.min_split)),
        None => {
            let file = File::create(&part)?;
            if let Some(size) = info.size {
                file.set_len(size)?;
            }
            (file, Table::whole(info.size.unwrap_or(u64::MAX), opts.min_split))
        }
    };
    let resumed = table.downloaded();
    // At least FLUSH, so every request still ends in whole buffered writes.
    let cap = opts.max_request.or_else(|| request_cap(&info.url)).map_or(u64::MAX, |c| c.max(FLUSH));
    let sh = Arc::new(Shared {
        table: Mutex::new(table),
        file: Arc::new(file),
        url: info.url,
        headers: info.headers,
        validator: info.validator,
        ranges,
        size: info.size,
        done: CancellationToken::new(),
        limiters: opts.limiters.clone(),
        cap,
        accepted: AtomicUsize::new(0),
        limited: AtomicBool::new(false),
        ramp: Notify::new(),
    });

    let mut workers = JoinSet::new();
    let max = if ranges { clients.len() } else { 1 };
    grow(&sh, &mut workers, &mut clients, max);

    let report = || {
        let t = sh.table.lock().unwrap();
        progress.send_replace(Progress {
            downloaded: t.downloaded(),
            total: info.size,
            connections: t.active(),
            segments: if ranges { t.segments().to_vec() } else { Vec::new() },
            ..Progress::default()
        });
    };
    let mut tick = interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_save = Instant::now();
    let mut error = None;
    loop {
        tokio::select! {
            joined = workers.join_next() => match joined {
                None => break,
                Some(Ok(Ok(()))) => {}
                // Nothing more of a changed file is worth having.
                Some(Ok(Err(e @ Error::Changed(_)))) => {
                    workers.shutdown().await;
                    error = Some(e);
                    break;
                }
                Some(Ok(Err(e))) => { error.get_or_insert(e); }
                Some(Err(e)) => std::panic::resume_unwind(e.into_panic()),
            },
            _ = stop.cancelled() => {
                workers.shutdown().await;
                report();
                if ranges {
                    save_state(&sh, &state_path, info.size.unwrap_or(0)).await?;
                }
                return Err(Error::Cancelled);
            }
            _ = sh.ramp.notified() => grow(&sh, &mut workers, &mut clients, max),
            _ = tick.tick() => {
                grow(&sh, &mut workers, &mut clients, max);
                report();
                if ranges && last_save.elapsed() >= SAVE_EVERY {
                    save_state(&sh, &state_path, info.size.unwrap_or(0)).await?;
                    last_save = Instant::now();
                }
            }
        }
    }
    report();

    let (complete, segments, size, refused) = {
        let t = sh.table.lock().unwrap();
        (t.is_complete(), t.len(), t.downloaded(), t.stepped_back())
    };
    // Some of the bytes may be of the new version: the next run must start fresh.
    if let Some(Error::Changed(_)) = &error {
        remove_if_exists(&state_path)?;
        return Err(error.expect("matched above"));
    }
    if !complete {
        if ranges {
            save_state(&sh, &state_path, info.size.unwrap_or(0)).await?;
        }
        return Err(error.unwrap_or(Error::Incomplete));
    }
    let file = Arc::into_inner(sh).and_then(|sh| Arc::into_inner(sh.file)).expect("workers are finished");
    file.sync_all()?;
    drop(file);
    // Before the file gets its name, so a file that does not match never looks finished.
    let checked = match &info.checksum {
        Some(sum) => {
            let (from, kind) = (part.clone(), sum.kind);
            let hashed = tokio::select! {
                hashed = spawn_blocking(move || hash_file(&from, kind)) => hashed.map_err(io::Error::other)??,
                // The saved state stays as it is: still right, if up to a second old.
                _ = stop.cancelled() => return Err(Error::Cancelled),
            };
            if hashed != sum.value {
                remove_if_exists(&state_path)?;
                return Err(Error::Checksum(format!("{} ({})", sum.kind.name(), sum.header)));
            }
            Some(sum.describe())
        }
        None => None,
    };
    let path = free_path(&opts.dir, &name);
    std::fs::rename(&part, &path)?;
    if state_path.exists() {
        std::fs::remove_file(&state_path)?;
    }
    Ok(Outcome { path, size, resumed, segments, ranges, refused, checked })
}

/// Opens connections as the server accepts them, as IDM does: FIRST_CONNECTIONS at first,
/// then up to twice as many as requests it has answered, up to `max`. The first refusal
/// (429 / 503) stops the growth, so a server that allows only a few connections sees one
/// or two extra requests rather than a burst that can get the whole address blocked for
/// minutes (Wikimedia: 2 connections, then 429 with Retry-After: 600).
fn grow(sh: &Arc<Shared>, workers: &mut JoinSet<Result<()>>, clients: &mut Vec<Client>, max: usize) {
    if sh.limited.load(Ordering::SeqCst) {
        return;
    }
    let allowed = (2 * sh.accepted.load(Ordering::SeqCst)).max(FIRST_CONNECTIONS).min(max);
    while workers.len() < allowed {
        let Some(client) = clients.pop() else { return };
        let Some(first) = sh.table.lock().unwrap().start(std::time::Instant::now()) else { return };
        workers.spawn(worker(sh.clone(), client, first));
    }
}

enum Fetched {
    Done,
    /// The server turned this connection away and it gave its segment back.
    SteppedBack,
}

/// One connection: works on `first`, then claims segments until no work is left.
async fn worker(sh: Arc<Shared>, client: Client, first: usize) -> Result<()> {
    let mut i = first;
    loop {
        match fetch_with_retries(&sh, &client, i).await {
            Ok(Fetched::Done) => {}
            Ok(Fetched::SteppedBack) => return Ok(()),
            Err(e) => {
                sh.table.lock().unwrap().leave(i);
                return Err(e);
            }
        }
        let speed = {
            let (bytes, since) = sh.table.lock().unwrap().since_claim(i);
            bytes as f64 / since.elapsed().max(SPEED_OVER).as_secs_f64()
        };
        let mut prev = Some(i);
        i = loop {
            let claim = sh.table.lock().unwrap().next(prev.take(), speed, std::time::Instant::now());
            match claim {
                Claim::Work(next) => break next,
                Claim::Wait => sleep(WAIT).await,
                Claim::Leave => return Ok(()),
            }
        };
    }
}

async fn fetch_with_retries(sh: &Shared, client: &Client, i: usize) -> Result<Fetched> {
    let mut failures = 0;
    loop {
        let before = sh.table.lock().unwrap().get(i).pos;
        let err = match fetch(sh, client, i).await {
            Ok(true) => return Ok(Fetched::Done),
            // A capped request ended; the next one continues the segment.
            Ok(false) => {
                failures = 0;
                continue;
            }
            Err(e) => e,
        };
        // A server that limits connections answers the extra ones with 429 / 503: settle
        // for fewer connections, like IDM does, instead of hammering it.
        if matches!(err, Error::Busy { .. }) {
            sh.limited.store(true, Ordering::SeqCst);
        }
        if matches!(err, Error::Busy { .. }) && sh.table.lock().unwrap().step_back(i) {
            return Ok(Fetched::SteppedBack);
        }
        if sh.table.lock().unwrap().get(i).pos > before {
            failures = 0;
        }
        failures += 1;
        if failures > MAX_RETRIES || !err.retryable() {
            return Err(err);
        }
        tokio::select! {
            _ = sleep(err.retry_delay(failures)) => {}
            _ = sh.done.cancelled() => return Ok(Fetched::Done),
        }
    }
}

/// One request for the rest of segment `i` (at most `cap` bytes of it), written as it
/// arrives. Returns true when the segment is finished, which can be early when another
/// connection took over its tail, and false when a capped request ended before that.
async fn fetch(sh: &Shared, client: &Client, i: usize) -> Result<bool> {
    if !sh.ranges {
        sh.table.lock().unwrap().reset(i);
    }
    let (pos, end) = {
        let t = sh.table.lock().unwrap();
        (t.get(i).pos, t.get(i).end)
    };
    if pos >= end {
        return Ok(true);
    }
    let until = end.min(pos.saturating_add(sh.cap));
    let mut req = client.get(sh.url.clone()).headers(sh.headers.clone());
    if sh.ranges {
        req = req.header(RANGE, format!("bytes={pos}-{}", until - 1));
        if let Some(v) = sh.validator.if_range() {
            req = req.header(IF_RANGE, v);
        }
    }
    let resp = tokio::select! {
        resp = timeout(STALL, req.send()) => resp.map_err(|_| Error::Stalled)??,
        _ = sh.done.cancelled() => return Ok(true),
    };
    let status = resp.status();
    // A 200 to If-Range is how a server says the file no longer matches its validator.
    if sh.ranges && status == StatusCode::OK && sh.validator.if_range().is_some() {
        return Err(Error::Changed("it no longer matches its ETag or date".into()));
    }
    let expected = if sh.ranges { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    if status != expected {
        return Err(if status.is_success() { Error::Range(format!("answered {status}")) } else { Error::from_response(&resp) });
    }
    if sh.ranges {
        let got = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range);
        // Also catches a server that has no validators.
        if let (Some((_, _, Some(total))), Some(size)) = (got, sh.size)
            && total != size
        {
            return Err(Error::Changed(format!("its size went from {size} to {total} bytes")));
        }
        if got.map(|r| r.0) != Some(pos) {
            return Err(Error::Range(format!("asked from {pos}, got {got:?}")));
        }
    }
    sh.accepted.fetch_add(1, Ordering::SeqCst);
    sh.ramp.notify_one();

    let mut body = resp.bytes_stream();
    let mut buf = BytesMut::new();
    let mut written = std::time::Instant::now();
    loop {
        let next = tokio::select! {
            next = timeout(STALL, body.next()) => next.map_err(|_| Error::Stalled)?,
            _ = sh.done.cancelled() => return Ok(true),
        };
        match next {
            Some(chunk) => {
                let chunk = chunk?;
                // What runs past the segment's end (moved closer by a split while this request
                // was out) is dropped below, so it does not count against the limit either.
                let kept = sh.table.lock().unwrap().get(i).remaining().saturating_sub(buf.len() as u64).min(chunk.len() as u64);
                // Each limit is charged at once; the longest wait covers them all.
                if let Some(wait) = sh.limiters.iter().filter_map(|limiter| limiter.reserve(kept)).max() {
                    // Waiting for its share of a low speed limit is not stalling.
                    sh.table.lock().unwrap().hold(i, true, std::time::Instant::now());
                    sleep(wait).await;
                    sh.table.lock().unwrap().hold(i, false, std::time::Instant::now());
                }
                buf.extend_from_slice(&chunk);
                // Flush as soon as the buffer holds everything the segment still needs: its
                // end moves closer when another connection takes over the tail, and reading
                // on until FLUSH bytes would waste up to that much past the new end. Also at
                // least once a FLUSH_AFTER while bytes come in.
                let due = written.elapsed() >= FLUSH_AFTER;
                while buf.len() as u64 >= FLUSH.min(sh.table.lock().unwrap().get(i).remaining()) || (due && !buf.is_empty()) {
                    if flush(sh, i, &mut buf).await? {
                        return Ok(true);
                    }
                    written = std::time::Instant::now();
                }
            }
            None => {
                while !buf.is_empty() {
                    if flush(sh, i, &mut buf).await? {
                        return Ok(true);
                    }
                }
                let mut t = sh.table.lock().unwrap();
                if t.get(i).end == u64::MAX {
                    t.close_at_pos(i);
                    return Ok(true);
                }
                if sh.ranges && t.get(i).pos >= until {
                    return Ok(false);
                }
                return Err(Error::ShortBody);
            }
        }
    }
}

/// Writes up to FLUSH buffered bytes at the segment's position, clipped to its current end.
/// Returns true once the segment is finished.
async fn flush(sh: &Shared, i: usize, buf: &mut BytesMut) -> Result<bool> {
    let (pos, n) = {
        let t = sh.table.lock().unwrap();
        let seg = t.get(i);
        (seg.pos, (buf.len() as u64).min(FLUSH).min(seg.remaining()))
    };
    if n == 0 {
        buf.clear();
        return Ok(true);
    }
    let data = buf.split_to(n as usize).freeze();
    let file = sh.file.clone();
    spawn_blocking(move || write_all_at(&file, &data, pos)).await.map_err(io::Error::other)??;
    let (done, all) = {
        let mut t = sh.table.lock().unwrap();
        (t.commit(i, n, std::time::Instant::now()), t.is_complete())
    };
    if all {
        sh.done.cancel();
    }
    if done {
        buf.clear();
    }
    Ok(done)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Flushes the data, then records the segments that were committed before the flush.
/// Written to a temporary file and renamed, so a kill at any moment leaves a valid state.
async fn save_state(sh: &Arc<Shared>, path: &Path, size: u64) -> Result<()> {
    let state = SavedState {
        url: sh.url.to_string(),
        size,
        validator: sh.validator.clone(),
        segments: sh.table.lock().unwrap().segments().to_vec(),
    };
    let json = serde_json::to_vec(&state)?;
    let file = sh.file.clone();
    let path = path.to_owned();
    spawn_blocking(move || -> io::Result<()> {
        file.sync_data()?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    })
    .await
    .map_err(io::Error::other)??;
    Ok(())
}

/// Segments of an earlier run, if its state matches the same remote file and the part file.
/// Under a new address the validators may differ (see `Options::new_address`).
fn load_state(path: &Path, part: &Path, size: Option<u64>, validator: &Validator, new_address: bool) -> Option<Vec<Segment>> {
    let state: SavedState = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let part_len = std::fs::metadata(part).ok()?.len();
    let fits = Some(state.size) == size && part_len == state.size && (new_address || state.validator.same_file(validator));
    fits.then_some(state.segments)
}

/// Whether the server still sends what the part file holds at the end of its longest
/// written run (`OVERLAP` bytes of it). ETag and date can stay the same while the file
/// changes, and some servers send neither. Nothing written yet passes.
async fn still_same(client: &Client, info: &Probe, part: &Path, segments: &[Segment]) -> Result<bool> {
    let Some(seg) = segments.iter().max_by_key(|s| s.pos - s.start) else { return Ok(true) };
    let (from, to) = (seg.pos.saturating_sub(OVERLAP).max(seg.start), seg.pos);
    if from == to {
        return Ok(true);
    }
    let mut on_disk = vec![0; (to - from) as usize];
    let mut file = File::open(part)?;
    file.seek(SeekFrom::Start(from))?;
    file.read_exact(&mut on_disk)?;
    let mut failures = 0;
    loop {
        match fetch_range(client, info, from, to).await {
            Ok(got) => return Ok(got.is_some_and(|got| got[..] == on_disk[..])),
            Err(e) if e.retryable() && failures < MAX_RETRIES => {
                failures += 1;
                sleep(e.retry_delay(failures)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Bytes `from..to`, or None when the server answers with anything but exactly that range
/// of a file of the probed size.
async fn fetch_range(client: &Client, info: &Probe, from: u64, to: u64) -> Result<Option<Bytes>> {
    let req = client.get(info.url.clone()).headers(info.headers.clone()).header(RANGE, format!("bytes={from}-{}", to - 1));
    let resp = timeout(STALL, req.send()).await.map_err(|_| Error::Stalled)??;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::from_response(&resp));
    }
    let got = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range);
    if status != StatusCode::PARTIAL_CONTENT || got != Some((from, to - 1, info.size)) {
        return Ok(None);
    }
    Ok(Some(timeout(STALL, resp.bytes()).await.map_err(|_| Error::Stalled)??))
}

pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `dir/name`, or `dir/stem (n).ext` when that is taken.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::{Url, request_cap};

    #[test]
    fn youtube_video_servers_get_a_request_cap() {
        let cap = |u: &str| request_cap(&Url::parse(u).unwrap());
        assert_eq!(cap("https://rr3---sn-abcdefgh-ijkl.googlevideo.com/videoplayback?itag=398"), Some(10 << 20));
        assert_eq!(cap("https://googlevideo.com/x"), Some(10 << 20));
        assert_eq!(cap("https://notgooglevideo.com/x"), None);
        assert_eq!(cap("https://static.rust-lang.org/dist/x.tar.xz"), None);
    }
}
