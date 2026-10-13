//! Streams delivered as many small pieces (HLS, DASH): downloaded over several connections at
//! once, written in order into one `.part` file, resumable piece by piece.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use reqwest::header::{CONTENT_RANGE, HeaderMap, RANGE};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::download::{Options, Outcome, Progress, STALL, free_path, remove_if_exists};
use crate::segments::TAKE_OVER_AFTER;
use crate::probe::{header_map, headers_for, parse_content_range};
use crate::segments::Segment;
use crate::{Error, Limiter, MAX_RETRIES, Result, client};

const SAVE_EVERY: Duration = Duration::from_secs(1);

/// One piece: a whole file, or a byte range of one.
#[derive(Debug, Clone)]
pub struct Piece {
    pub url: Url,
    /// `(start, length)`.
    pub range: Option<(u64, u64)>,
    /// AES-128-CBC key for the piece (plain HLS encryption, not DRM).
    pub key: Option<Key>,
    /// Seconds of media.
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub url: Url,
    pub iv: [u8; 16],
}

/// Everything one stream is made of, in playing order.
#[derive(Debug, Clone)]
pub struct Pieces {
    /// Where the list came from; cookies and credentials only go to its host.
    pub source: Url,
    pub(crate) headers: HeaderMap,
    /// Initialisation section (fMP4), written first.
    pub init: Option<Piece>,
    pub pieces: Vec<Piece>,
    /// MPEG-TS pieces, which may come disguised behind a fake image header.
    pub ts: bool,
    /// Expected size before any piece arrived (bitrate x duration).
    pub size_hint: Option<u64>,
    /// The last piece may not exist: a DASH manifest's pieces are counted from the video's
    /// length, rounded up, and its streams are not all that long. A 404 or 410 for it ends
    /// the stream there, as players and yt-dlp take it.
    pub open_end: bool,
}

impl Pieces {
    /// A list read elsewhere (DASH pieces from yt-dlp): `(address, byte range)` per piece,
    /// requested with `headers` (as given for `source`).
    pub fn new(
        source: &str,
        headers: &[(String, String)],
        pieces: Vec<(String, Option<(u64, u64)>)>,
    ) -> Result<Pieces> {
        let parse = |u: &str| Url::parse(u).map_err(|_| Error::Playlist(format!("bad piece address {u}")));
        let pieces = pieces
            .into_iter()
            .map(|(url, range)| Ok(Piece { url: parse(&url)?, range, key: None, duration: 0.0 }))
            .collect::<Result<Vec<_>>>()?;
        Ok(Pieces {
            source: parse(source)?,
            headers: header_map(headers)?,
            init: None,
            pieces,
            ts: false,
            size_hint: None,
            open_end: true,
        })
    }

    /// Seconds of media in all pieces.
    pub fn duration(&self) -> f64 {
        self.pieces.iter().map(|p| p.duration).sum()
    }

    /// The exact size when every piece is a byte range, else `size_hint`.
    pub fn expected_size(&self) -> Option<u64> {
        self.exact_size().or(self.size_hint)
    }

    /// The size, known only when every piece is a byte range.
    pub fn exact_size(&self) -> Option<u64> {
        self.init.iter().chain(&self.pieces).map(|p| p.range.map(|r| r.1)).sum()
    }

    /// Requests the stream takes: the initialisation section and every piece.
    pub fn count(&self) -> usize {
        usize::from(self.init.is_some()) + self.pieces.len()
    }
}

/// Saved next to the `.part` file: the first `written` pieces fill its first `len` bytes.
#[derive(Serialize, Deserialize)]
struct SavedPieces {
    count: usize,
    written: usize,
    len: u64,
}

struct Shared {
    list: Pieces,
    jobs: Vec<Piece>,
    limiters: Vec<std::sync::Arc<Limiter>>,
    keys: Mutex<HashMap<Url, [u8; 16]>>,
    /// Bytes received for pieces not written yet, for a smooth progress figure.
    in_flight: AtomicU64,
    /// Requests open right now (a piece received and waiting for an earlier one is not).
    receiving: AtomicUsize,
    /// Per connection: when its request last received bytes (milliseconds since `started`),
    /// to see a piece that stalls.
    last_byte: Vec<AtomicU64>,
    /// Per connection: bytes its request received so far, counted as they arrive (its
    /// speed), and whether it waits on the speed limit right now (left alone meanwhile).
    received: Vec<AtomicU64>,
    held: Vec<AtomicBool>,
    started: Instant,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Connection `client` starts a request: nothing received yet, heard from now.
    fn begin(&self, client: usize) {
        self.last_byte[client].store(self.now_ms(), Ordering::Relaxed);
        self.received[client].store(0, Ordering::Relaxed);
        self.held[client].store(false, Ordering::Relaxed);
    }
}

/// Whether the next piece to write (the head, on one request `age` old) is fetched again on
/// a free connection: once its request is TAKE_OVER_AFTER old, when it received nothing for
/// that long, or when the pieces that overtook it (`overtakers`: the slowest of their
/// speeds) were all at least twice as fast as it is, so evenly slow pieces that finish out
/// of order by chance are not fetched twice. Never while it waits on the speed limit
/// (`held`): the limit is what holds it back, and a second request would only share it.
fn fetch_head_again(age: Duration, quiet_for: Duration, held: bool, head_speed: f64, overtakers: Option<f64>) -> bool {
    !held
        && age >= TAKE_OVER_AFTER
        && (quiet_for >= TAKE_OVER_AFTER || overtakers.is_some_and(|slowest| head_speed * 2.0 < slowest))
}

/// One request for a piece: the piece, the connection, and when it started.
struct Attempt {
    client: usize,
    since: Instant,
    cancel: CancellationToken,
}

/// Downloads every piece into `opts.dir/opts.file_name`. Cancelling `stop` saves the state
/// and returns `Error::Cancelled`; the same call with the same list later resumes.
pub async fn download_pieces(
    list: Pieces,
    opts: &Options,
    progress: &watch::Sender<Progress>,
    stop: &CancellationToken,
) -> Result<Outcome> {
    let name = opts.file_name.clone().unwrap_or_else(|| "video.ts".to_owned());
    std::fs::create_dir_all(&opts.dir)?;
    let part = opts.dir.join(format!("{name}.part"));
    let state_path = opts.dir.join(format!("{name}.part.json"));
    let jobs: Vec<Piece> = list.init.iter().chain(&list.pieces).cloned().collect();
    let count = jobs.len();

    let exact = list.exact_size();
    let init_count = usize::from(list.init.is_some());
    let size_hint = list.size_hint;
    // One client per connection, as for files, each with one request at a time.
    let n = opts.connections.max(1);
    let sh = Shared {
        list,
        jobs,
        limiters: opts.limiters.clone(),
        keys: Mutex::new(HashMap::new()),
        in_flight: AtomicU64::new(0),
        receiving: AtomicUsize::new(0),
        last_byte: (0..n).map(|_| AtomicU64::new(0)).collect(),
        received: (0..n).map(|_| AtomicU64::new(0)).collect(),
        held: (0..n).map(|_| AtomicBool::new(false)).collect(),
        started: Instant::now(),
    };
    let clients = (0..n).map(|_| client()).collect::<Result<Vec<_>>>()?;

    let saved = match load_state(&state_path, &part, count) {
        Some(saved) if saved.written > 0 => tokio::select! {
            same = last_piece_same(&sh, &clients[0], &part, &saved) => same?.then_some(saved),
            _ = stop.cancelled() => return Err(Error::Cancelled),
        },
        saved => saved,
    };
    if saved.is_none() {
        remove_if_exists(&state_path)?;
    }
    let mut file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(saved.is_none()).open(&part).await?;
    let (mut written, mut len) = saved.map_or((0, 0), |s| (s.written, s.len));
    file.set_len(len).await?;
    file.seek(SeekFrom::Start(len)).await?;
    let resumed = len;

    let estimate = |written: usize, len: u64| {
        exact.or_else(|| {
            let media = written.saturating_sub(init_count);
            (media > 0).then(|| len * (count - init_count) as u64 / media as u64).or(size_hint)
        })
    };
    let report = |written: usize, len: u64| {
        let downloaded = len + sh.in_flight.load(Ordering::Relaxed);
        let total = estimate(written, len).map(|t| t.max(downloaded));
        progress.send_replace(Progress {
            downloaded,
            total,
            estimated: exact.is_none() && written < count,
            connections: sh.receiving.load(Ordering::Relaxed),
            // Pieces are written in order: one bar for the part on disk.
            segments: vec![Segment { start: 0, pos: downloaded, end: total.unwrap_or(downloaded) }],
            pieces: Some((written, count)),
        });
    };

    // Pieces arrive in any order and are written in order. Free connections fetch ahead, up
    // to `window` pieces past the file's end, so a slow piece does not hold the others back;
    // the next piece to write, when slow, is fetched again on a free connection and the
    // first copy to arrive is kept (as segments are taken over in plain files).
    let window = 2 * n;
    let start = |i: usize, client: usize, cancel: CancellationToken| attempt(&sh, &clients[client], client, i, cancel);
    let mut fetched = FuturesUnordered::new();
    let mut attempts: HashMap<usize, Vec<Attempt>> = HashMap::new();
    // Pieces in before the head, with the speed each came at.
    let mut ready: BTreeMap<usize, (Bytes, Counted<'_>, f64)> = BTreeMap::new();
    let mut free: Vec<usize> = (0..n).rev().collect();
    let mut queued = written;

    let mut tick = interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_save = Instant::now();
    while written < count {
        while queued < count && queued - written < window {
            let Some(client) = free.pop() else { break };
            let cancel = CancellationToken::new();
            sh.begin(client);
            attempts.entry(queued).or_default().push(Attempt { client, since: Instant::now(), cancel: cancel.clone() });
            fetched.push(start(queued, client, cancel));
            queued += 1;
        }
        if fetched.is_empty() {
            // Every piece before `queued` is written, waiting, or being fetched.
            return Err(Error::Incomplete);
        }
        tokio::select! {
            Some((i, client, result)) = fetched.next() => {
                free.push(client);
                let mut since = None;
                let others = attempts.get_mut(&i).map_or(0, |a| {
                    since = a.iter().find(|t| t.client == client).map(|t| t.since);
                    a.retain(|t| t.client != client);
                    a.len()
                });
                match result {
                    // A second copy of a piece that is already in.
                    _ if i < written || ready.contains_key(&i) => {}
                    Ok((data, counted)) => {
                        for other in attempts.remove(&i).unwrap_or_default() {
                            other.cancel.cancel();
                        }
                        // Unknown (no attempt on record): taken as no speed, which never makes
                        // another piece look slow.
                        let speed = since.map_or(0.0, |s| data.len() as f64 / s.elapsed().as_secs_f64().max(0.001));
                        ready.insert(i, (data, counted, speed));
                        while let Some((data, counted, _)) = ready.remove(&written) {
                            file.write_all(&data).await?;
                            drop(counted);
                            written += 1;
                            len += data.len() as u64;
                        }
                        if last_save.elapsed() >= SAVE_EVERY {
                            save_state(&mut file, &state_path, count, written, len).await?;
                            last_save = Instant::now();
                        }
                    }
                    // The other request for it may still bring it.
                    Err(_) if others > 0 => {}
                    Err(e) => {
                        drop(fetched);
                        save_state(&mut file, &state_path, count, written, len).await?;
                        return Err(e);
                    }
                }
                if attempts.get(&i).is_some_and(Vec::is_empty) {
                    attempts.remove(&i);
                }
            }
            _ = stop.cancelled() => {
                drop(fetched);
                save_state(&mut file, &state_path, count, written, len).await?;
                return Err(Error::Cancelled);
            }
            _ = tick.tick() => {
                report(written, len);
                // The next piece to write holds the file back: fetched again when it is
                // far slower than the pieces that overtook it, or received nothing for a while.
                if let Some(head) = attempts.get_mut(&written)
                    && let [only] = head.as_slice()
                    && let Some(&client) = free.last()
                {
                    let age = only.since.elapsed();
                    let quiet_for = Duration::from_millis(sh.now_ms().saturating_sub(sh.last_byte[only.client].load(Ordering::Relaxed)));
                    let held = sh.held[only.client].load(Ordering::Relaxed);
                    let head_speed = sh.received[only.client].load(Ordering::Relaxed) as f64 / age.as_secs_f64().max(0.001);
                    let overtakers = ready.values().map(|r| r.2).reduce(f64::min);
                    if fetch_head_again(age, quiet_for, held, head_speed, overtakers) {
                        free.pop();
                        let cancel = CancellationToken::new();
                        sh.begin(client);
                        head.push(Attempt { client, since: Instant::now(), cancel: cancel.clone() });
                        fetched.push(start(written, client, cancel));
                    }
                }
            }
        }
    }
    drop(fetched);
    drop(ready);
    sh.in_flight.store(0, Ordering::Relaxed);
    report(written, len);
    file.sync_all().await?;
    drop(file);
    let path = free_path(&opts.dir, &name);
    std::fs::rename(&part, &path)?;
    if state_path.exists() {
        std::fs::remove_file(&state_path)?;
    }
    Ok(Outcome { path, size: len, resumed, segments: count, ranges: true, refused: 0, checked: None })
}

/// One request for piece `i` on connection `client`, given up when `cancel` fires (another
/// copy of the piece came first): the piece, the connection, and what it brought.
async fn attempt<'a>(
    sh: &'a Shared,
    client: &'a Client,
    index: usize,
    i: usize,
    cancel: CancellationToken,
) -> (usize, usize, Result<(Bytes, Counted<'a>)>) {
    let result = tokio::select! {
        result = fetch_piece(sh, client, index, i) => result,
        _ = cancel.cancelled() => Err(Error::Cancelled),
    };
    (i, index, result)
}

/// Bytes received for a piece not written yet, counted in `in_flight` until this is dropped:
/// once the piece is written, or when its request is given up or cancelled.
struct Counted<'a> {
    total: &'a AtomicU64,
    bytes: u64,
}

impl<'a> Counted<'a> {
    fn new(total: &'a AtomicU64) -> Self {
        Counted { total, bytes: 0 }
    }

    fn add(&mut self, n: u64) {
        self.bytes += n;
        self.total.fetch_add(n, Ordering::Relaxed);
    }

    fn clear(&mut self) {
        self.total.fetch_sub(std::mem::take(&mut self.bytes), Ordering::Relaxed);
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Piece `i` over connection `index`, decrypted and unwrapped, with its bytes still counted
/// as received.
async fn fetch_piece<'a>(sh: &'a Shared, client: &Client, index: usize, i: usize) -> Result<(Bytes, Counted<'a>)> {
    let piece = &sh.jobs[i];
    let mut failures = 0;
    let mut counted = Counted::new(&sh.in_flight);
    let raw = loop {
        match fetch_once(sh, client, index, piece, &mut counted).await {
            Ok(data) => break data,
            Err(e) => {
                counted.clear();
                // The speed counts what the try that brings the piece receives.
                sh.received[index].store(0, Ordering::Relaxed);
                failures += 1;
                let gone = matches!(e, Error::Status(StatusCode::NOT_FOUND | StatusCode::GONE));
                if gone && sh.list.open_end && i > 0 && i + 1 == sh.jobs.len() {
                    break Bytes::new();
                }
                if failures > MAX_RETRIES || !e.retryable() {
                    return Err(e);
                }
                sleep(e.retry_delay(failures)).await;
            }
        }
    };
    let mut data = match &piece.key {
        Some(key) => decrypt(raw, &sh.key(client, &key.url).await?, &key.iv)?,
        None => raw,
    };
    if sh.list.ts && i >= usize::from(sh.list.init.is_some()) {
        data = skip_disguise(data);
    }
    Ok((data, counted))
}

/// Whether the last piece written in an earlier run is still what the server sends for it.
/// A list with the same number of pieces can still be of another file (a new encode, or
/// another rendition behind the same address), and mixing them would ruin the video.
async fn last_piece_same(sh: &Shared, client: &Client, part: &Path, saved: &SavedPieces) -> Result<bool> {
    let (data, _counted) = fetch_piece(sh, client, 0, saved.written - 1).await?;
    let Some(from) = saved.len.checked_sub(data.len() as u64) else { return Ok(false) };
    let mut on_disk = vec![0; data.len()];
    let mut file = std::fs::File::open(part)?;
    file.seek(SeekFrom::Start(from))?;
    file.read_exact(&mut on_disk)?;
    Ok(on_disk[..] == data[..])
}

impl Shared {
    /// The 16-byte key at `url`, fetched once.
    async fn key(&self, client: &Client, url: &Url) -> Result<[u8; 16]> {
        if let Some(key) = self.keys.lock().unwrap().get(url) {
            return Ok(*key);
        }
        let mut failures = 0;
        let key = loop {
            let headers = headers_for(url, Some(&self.list.source), self.list.headers.clone());
            let attempt = async {
                let resp = client.get(url.clone()).headers(headers).send().await?;
                if !resp.status().is_success() {
                    return Err(Error::from_response(&resp));
                }
                Ok(resp.bytes().await?)
            };
            match timeout(STALL, attempt).await.map_err(|_| Error::Stalled).and_then(|r| r) {
                Ok(body) => {
                    break <[u8; 16]>::try_from(body.as_ref())
                        .map_err(|_| Error::Playlist(format!("the AES-128 key is {} bytes, not 16", body.len())))?;
                }
                Err(e) => {
                    failures += 1;
                    if failures > MAX_RETRIES || !e.retryable() {
                        return Err(e);
                    }
                    sleep(e.retry_delay(failures)).await;
                }
            }
        };
        self.keys.lock().unwrap().insert(url.clone(), key);
        Ok(key)
    }
}

async fn fetch_once(sh: &Shared, client: &Client, index: usize, piece: &Piece, counted: &mut Counted<'_>) -> Result<Bytes> {
    let headers = headers_for(&piece.url, Some(&sh.list.source), sh.list.headers.clone());
    let mut req = client.get(piece.url.clone()).headers(headers);
    if let Some((start, len)) = piece.range {
        req = req.header(RANGE, format!("bytes={start}-{}", start + len - 1));
    }
    let _open = Receiving::new(&sh.receiving);
    let resp = timeout(STALL, req.send()).await.map_err(|_| Error::Stalled)??;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::from_response(&resp));
    }
    if let (StatusCode::PARTIAL_CONTENT, Some((start, _))) = (status, piece.range)
        && let Some(got) = resp.headers().get(CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range)
        && got.0 != start
    {
        return Err(Error::Range(format!("asked for a piece from {start}, got {}-{}", got.0, got.1)));
    }
    let mut body = resp.bytes_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = timeout(STALL, body.next()).await.map_err(|_| Error::Stalled)? {
        let chunk = chunk?;
        sh.received[index].fetch_add(chunk.len() as u64, Ordering::Relaxed);
        sh.last_byte[index].store(sh.now_ms(), Ordering::Relaxed);
        // Each limit is charged at once; the longest wait covers them all.
        if let Some(wait) = sh.limiters.iter().filter_map(|limiter| limiter.reserve(chunk.len() as u64)).max() {
            // Waiting for its share of the speed limit is not silence.
            sh.held[index].store(true, Ordering::Relaxed);
            sleep(wait).await;
            sh.held[index].store(false, Ordering::Relaxed);
            sh.last_byte[index].store(sh.now_ms(), Ordering::Relaxed);
        }
        counted.add(chunk.len() as u64);
        buf.extend_from_slice(&chunk);
    }
    let mut data = buf.freeze();
    if let Some((start, len)) = piece.range {
        // A server that ignores the range sends the whole file; cut the piece out of it.
        if status == StatusCode::OK && data.len() as u64 > len {
            let end = start.saturating_add(len);
            if end > data.len() as u64 {
                return Err(Error::ShortBody);
            }
            data = data.slice(start as usize..end as usize);
        }
        if data.len() as u64 != len {
            return Err(Error::ShortBody);
        }
    }
    Ok(data)
}

/// Counts an open request for as long as it lives, also when its future is dropped.
struct Receiving<'a>(&'a AtomicUsize);

impl<'a> Receiving<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count)
    }
}

impl Drop for Receiving<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn decrypt(data: Bytes, key: &[u8; 16], iv: &[u8; 16]) -> Result<Bytes> {
    let mut buf = data.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| Error::Decrypt)?
        .len();
    buf.truncate(plain);
    Ok(Bytes::from(buf))
}

/// Some sites put MPEG-TS pieces behind a fake image header (a PNG or JPEG in front of the
/// data) so they pass as pictures. TS packets are 188 bytes, each starting with 0x47; data
/// that does not start that way is cut back to the first place where five packets in a
/// row start correctly. Anything else is left as it is.
fn skip_disguise(data: Bytes) -> Bytes {
    const SYNC: u8 = 0x47;
    const PACKET: usize = 188;
    const CHECK: usize = 5;
    if data.first() == Some(&SYNC) {
        return data;
    }
    let limit = data.len().saturating_sub(PACKET * (CHECK - 1)).min(64 * 1024);
    match (1..limit).find(|&k| (0..CHECK).all(|n| data[k + n * PACKET] == SYNC)) {
        Some(k) => data.slice(k..),
        None => data,
    }
}

async fn save_state(
    file: &mut tokio::fs::File,
    path: &std::path::Path,
    count: usize,
    written: usize,
    len: u64,
) -> Result<()> {
    file.flush().await?;
    file.sync_data().await?;
    let json = serde_json::to_vec(&SavedPieces { count, written, len })?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// The state of an earlier run, if it is for a list of the same length and the part file
/// holds at least what it says.
fn load_state(path: &std::path::Path, part: &std::path::Path, count: usize) -> Option<SavedPieces> {
    let state: SavedPieces = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let part_len = std::fs::metadata(part).ok()?.len();
    (state.count == count && state.written <= count && part_len >= state.len).then_some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disguised_transport_stream_is_unwrapped() {
        let mut ts = Vec::new();
        for n in 0..6u8 {
            ts.push(0x47);
            ts.extend(std::iter::repeat_n(n, 187));
        }
        let mut disguised = b"\x89PNG\r\n\x1a\n fake picture".to_vec();
        disguised.extend(&ts);
        assert_eq!(skip_disguise(Bytes::from(disguised)), Bytes::from(ts.clone()));
        assert_eq!(skip_disguise(Bytes::from(ts.clone())), Bytes::from(ts), "plain TS untouched");
        let other = Bytes::from_static(b"ID3\x04 packed audio");
        assert_eq!(skip_disguise(other.clone()), other, "no TS inside: untouched");
    }

    #[test]
    fn aes_128_round_trip() {
        use aes::cipher::BlockEncryptMut;
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let plain = b"exactly the bytes of one piece, any length".to_vec();
        let mut buf = plain.clone();
        buf.resize(plain.len() + 16, 0);
        let sealed = cbc::Encryptor::<aes::Aes128>::new((&key).into(), (&iv).into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        assert_eq!(decrypt(Bytes::from(sealed.clone()), &key, &iv).unwrap(), Bytes::from(plain));
        assert!(matches!(decrypt(Bytes::from(sealed), &[8u8; 16], &iv), Err(Error::Decrypt)));
    }

    /// The head piece is fetched again only once its request is TAKE_OVER_AFTER old and it
    /// does not wait on the speed limit: when it went quiet, or when the pieces that overtook
    /// it were all at least twice as fast.
    #[test]
    fn head_piece_fetched_again_only_when_it_lags() {
        let (young, old, short, long) = (TAKE_OVER_AFTER / 2, TAKE_OVER_AFTER, TAKE_OVER_AFTER / 3, TAKE_OVER_AFTER);
        // Quiet for long: again, unless it is too young.
        assert!(fetch_head_again(old, long, false, 0.0, None));
        assert!(!fetch_head_again(young, long, false, 0.0, None));
        // Receiving: not overtaken, or only by pieces about as fast (out of order by
        // chance), or by one not twice as fast: no.
        assert!(!fetch_head_again(old, short, false, 100.0, None));
        assert!(!fetch_head_again(old, short, false, 100.0, Some(150.0)));
        assert!(!fetch_head_again(old, short, false, 100.0, Some(199.0)));
        assert!(fetch_head_again(old, short, false, 100.0, Some(250.0)));
        assert!(!fetch_head_again(young, short, false, 100.0, Some(1e9)));
        // Waiting on the speed limit: never, however quiet or slow.
        assert!(!fetch_head_again(old, long, true, 0.0, None));
        assert!(!fetch_head_again(old, long, true, 100.0, Some(1e9)));
    }
}
