//! Local HTTP server for repeatable download tests and benchmarks.
//!
//! `GET /data` serves deterministic bytes. Query parameters:
//! - `size`: bytes to serve (default: all generated data)
//! - `ranges=0`: ignore Range headers and always answer 200
//! - `rate`: bytes per second for each response (default: unthrottled); its first 16 KiB
//!   are sent at once, so shorter answers are not slowed
//! - `name`: send a Content-Disposition header with this file name
//! - `etag`: ETag value (default `v1`)
//! - `cut` + `cuts`: the first `cuts` responses drop the connection after `cut` bytes
//! - `limit`: answer 503 + `Retry-After: 1` while `limit` bodies are already being sent
//!   (a per-client connection limit)
//! - `busy`: answer 429 + `Retry-After: 1` to the first `busy` requests
//! - `cookie`: answer 403 unless the request has exactly this Cookie header
//! - `type`: Content-Type header to send
//! - `crawl` + `crawl_rate`: the first `crawl` responses to a range that does not start at
//!   0 are sent at `crawl_rate` bytes per second (one slow connection among fast ones)
//! - `widest`: answer 403 to a closed range (`bytes=a-b`) wider than this many bytes, as
//!   YouTube's video servers do; open-ended ranges still pass
//! - `after` + `then_size` / `then_etag`: from response number `after` on (counting from 0,
//!   probes included), the file has this size / ETag (a file replaced on the server), and
//!   with `then_variant=1` other bytes too ([`variant`])
//! - `variant=1`: other bytes ([`variant`]) of the same size, with the same ETag and date
//! - `header` + `value`: one more response header (a hash, say)
//!
//! A request with If-Range gets its range only while that is the current ETag or date,
//! otherwise the whole file with 200.
//!
//! `GET /redirect?<query>` redirects to `/data?<query>`, with the header `hop_header`
//! set to `hop_value` when given.
//!
//! `GET /hls/index.m3u8` serves an HLS media playlist over the same data. Query parameters:
//! - `pieces` (default 4) and `size` (bytes per piece, default 1000)
//! - `key=seq` / `key=iv`: AES-128 pieces, IV from the sequence number (starting at 3) or
//!   given in the playlist; the key is at `/hls/key` ([`KEY`])
//! - `disguise=1`: pieces are MPEG-TS packets behind a fake PNG header ([`Server::piece`]
//!   gives the packets)
//! - `map=1`: an init section plus pieces as byte ranges of `/data` (the stream is then the
//!   first `(pieces + 1) * size` bytes of the data)
//! - `fail`: piece number that always answers 404
//! - `slow` + `slow_rate`: piece number sent at `slow_rate` bytes per second the first time
//!   it is asked for (one slow piece among fast ones)
//! - `rate`: bytes per second for each piece (also with `map=1`)
//! - `live=1`: no end tag
//! - `variant=1`: pieces of other bytes ([`variant`]), same sizes

mod folder;

pub use folder::Folder;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use bytes::Bytes;
use tokio::time::Instant;

const CHUNK: usize = 16 * 1024;

pub struct Server {
    pub addr: SocketAddr,
    pub data: Bytes,
    shared: Arc<Shared>,
}

struct Shared {
    data: Bytes,
    responses: AtomicUsize,
    in_flight: AtomicUsize,
    crawled: AtomicUsize,
    slowed: AtomicUsize,
    pieces: AtomicUsize,
    sent: AtomicU64,
}

/// Counts a body as in flight until its stream is dropped.
struct InFlight(Arc<Shared>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Server {
    /// URL of `path` (for example `/data?size=100`) on this server.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Number of `/data` responses served so far.
    pub fn responses(&self) -> usize {
        self.shared.responses.load(Ordering::SeqCst)
    }

    /// Bytes handed out in response bodies so far: what a download costs the server, so a
    /// test can tell one that went on from one that started over.
    pub fn bytes_sent(&self) -> u64 {
        self.shared.sent.load(Ordering::SeqCst)
    }

    /// Number of HLS pieces served so far (`/hls/piece`).
    pub fn pieces_served(&self) -> usize {
        self.shared.pieces.load(Ordering::SeqCst)
    }
}

impl Server {
    /// The unencrypted bytes of HLS piece `n` (as `/hls/piece` serves it before encryption
    /// and disguise): `size` bytes of the data, or with `ts` those bytes split into MPEG-TS
    /// packets of 187 bytes after a 0x47 sync byte.
    pub fn piece(&self, n: usize, size: usize, ts: bool) -> Vec<u8> {
        piece_bytes(&self.data, n, size, ts)
    }
}

/// AES-128 key of the encrypted HLS test streams.
pub const KEY: [u8; 16] = *b"rdm test key 16!";
/// The first piece's sequence number in the test playlists.
const FIRST_SEQUENCE: u64 = 3;
const EXPLICIT_IV: u128 = 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff;

fn piece_bytes(data: &[u8], n: usize, size: usize, ts: bool) -> Vec<u8> {
    let start = (n * size).min(data.len());
    let slice = &data[start..(start + size).min(data.len())];
    if !ts {
        return slice.to_vec();
    }
    slice.chunks(187).flat_map(|c| std::iter::once(0x47).chain(c.iter().copied())).collect()
}

#[derive(serde::Deserialize)]
struct HlsQuery {
    pieces: Option<usize>,
    size: Option<usize>,
    key: Option<String>,
    disguise: Option<u8>,
    map: Option<u8>,
    fail: Option<usize>,
    slow: Option<usize>,
    slow_rate: Option<u64>,
    rate: Option<u64>,
    live: Option<u8>,
    variant: Option<u8>,
}

async fn hls_playlist(Query(q): Query<HlsQuery>) -> Response {
    let (pieces, size) = (q.pieces.unwrap_or(4), q.size.unwrap_or(1000));
    let mut lines = vec![
        "#EXTM3U".to_owned(),
        "#EXT-X-VERSION:7".into(),
        "#EXT-X-TARGETDURATION:2".into(),
        format!("#EXT-X-MEDIA-SEQUENCE:{FIRST_SEQUENCE}"),
    ];
    if q.map == Some(1) {
        let data = q.rate.map_or("/data".to_owned(), |r| format!("/data?rate={r}"));
        lines.push(format!("#EXT-X-MAP:URI=\"{data}\",BYTERANGE=\"{size}@0\""));
        for n in 0..pieces {
            // The first piece gives its offset; the others continue where it ended.
            let offset = if n == 0 { format!("@{size}") } else { String::new() };
            lines.extend(["#EXTINF:2.0,".into(), format!("#EXT-X-BYTERANGE:{size}{offset}"), data.clone()]);
        }
    } else {
        match q.key.as_deref() {
            Some("seq") => lines.push("#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/key\"".into()),
            Some("iv") => lines.push(format!("#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/key\",IV=0x{EXPLICIT_IV:032x}")),
            _ => {}
        }
        let mut extra = String::new();
        let passed = [
            ("key", q.key.clone()),
            ("disguise", q.disguise.map(|d| d.to_string())),
            ("fail", q.fail.map(|f| f.to_string())),
            ("slow", q.slow.map(|s| s.to_string())),
            ("slow_rate", q.slow_rate.map(|r| r.to_string())),
            ("rate", q.rate.map(|r| r.to_string())),
            ("variant", q.variant.map(|v| v.to_string())),
        ];
        for (name, value) in passed {
            if let Some(v) = value {
                extra += &format!("&{name}={v}");
            }
        }
        for n in 0..pieces {
            lines.extend(["#EXTINF:2.0,".into(), format!("piece?n={n}&size={size}{extra}")]);
        }
    }
    if q.live != Some(1) {
        lines.push("#EXT-X-ENDLIST".into());
    }
    let body = lines.join("\n") + "\n";
    ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], body).into_response()
}

#[derive(serde::Deserialize)]
struct PieceQuery {
    n: usize,
    size: usize,
    key: Option<String>,
    disguise: Option<u8>,
    fail: Option<usize>,
    slow: Option<usize>,
    slow_rate: Option<u64>,
    rate: Option<u64>,
    variant: Option<u8>,
}

async fn hls_piece(State(shared): State<Arc<Shared>>, Query(q): Query<PieceQuery>) -> Response {
    use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    shared.pieces.fetch_add(1, Ordering::SeqCst);
    if q.fail == Some(q.n) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let disguise = q.disguise == Some(1);
    let mut body = piece_bytes(&shared.data, q.n, q.size, disguise);
    if q.variant == Some(1) {
        body = variant(&body);
    }
    if disguise {
        let mut fake = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR not really a picture".to_vec();
        fake.append(&mut body);
        body = fake;
    }
    let iv = match q.key.as_deref() {
        Some("seq") => Some((FIRST_SEQUENCE as u128 + q.n as u128).to_be_bytes()),
        Some("iv") => Some(EXPLICIT_IV.to_be_bytes()),
        _ => None,
    };
    if let Some(iv) = iv {
        body = cbc::Encryptor::<aes::Aes128>::new((&KEY).into(), (&iv).into()).encrypt_padded_vec_mut::<Pkcs7>(&body);
    }
    let guard = {
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(shared.clone())
    };
    let len = body.len();
    let slowed = q.slow == Some(q.n) && shared.slowed.fetch_add(1, Ordering::SeqCst) == 0;
    let rate = if slowed { q.slow_rate } else { q.rate };
    let stream = body_stream(Bytes::from(body), rate.filter(|&r| r > 0), None, guard);
    Response::builder().header(header::CONTENT_LENGTH, len).body(Body::from_stream(stream)).unwrap()
}

/// Starts a server on `addr` (use port 0 for a free port) serving `size` generated bytes.
pub async fn start(addr: SocketAddr, size: usize) -> std::io::Result<Server> {
    let data = generate(size);
    let shared = Arc::new(Shared { data: data.clone(), responses: AtomicUsize::new(0), in_flight: AtomicUsize::new(0), crawled: AtomicUsize::new(0), slowed: AtomicUsize::new(0), pieces: AtomicUsize::new(0), sent: AtomicU64::new(0) });
    let app = Router::new()
        .route("/data", get(serve_data))
        .route("/redirect", get(redirect))
        .route("/hls/index.m3u8", get(hls_playlist))
        .route("/hls/piece", get(hls_piece))
        .route("/hls/key", get(|| async { Bytes::from_static(&KEY) }))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok(Server { addr, data, shared })
}

/// Another version of `data` of the same length: every byte changed.
pub fn variant(data: &[u8]) -> Vec<u8> {
    data.iter().map(|b| b ^ 0x5a).collect()
}

/// Deterministic pseudo-random bytes (xorshift64), so every byte position is distinct enough
/// that misplaced writes show up as mismatches.
fn generate(size: usize) -> Bytes {
    let mut out = Vec::with_capacity(size + 8);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    while out.len() < size {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(size);
    Bytes::from(out)
}

#[derive(serde::Deserialize)]
struct DataQuery {
    size: Option<usize>,
    ranges: Option<u8>,
    rate: Option<u64>,
    name: Option<String>,
    etag: Option<String>,
    cut: Option<usize>,
    cuts: Option<usize>,
    limit: Option<usize>,
    busy: Option<usize>,
    cookie: Option<String>,
    #[serde(rename = "type")]
    content_type: Option<String>,
    widest: Option<usize>,
    crawl: Option<usize>,
    crawl_rate: Option<u64>,
    after: Option<usize>,
    then_size: Option<usize>,
    then_etag: Option<String>,
    then_variant: Option<u8>,
    variant: Option<u8>,
    header: Option<String>,
    value: Option<String>,
}

#[derive(serde::Deserialize)]
struct RedirectQuery {
    hop_header: Option<String>,
    hop_value: Option<String>,
}

const LAST_MODIFIED: &str = "Wed, 07 Oct 2026 12:00:00 GMT";

async fn redirect(Query(q): Query<RedirectQuery>, RawQuery(query): RawQuery) -> Response {
    let mut resp = Redirect::to(&format!("/data?{}", query.unwrap_or_default())).into_response();
    if let (Some(name), Some(value)) = (q.hop_header, q.hop_value) {
        resp.headers_mut().insert(header::HeaderName::try_from(name).unwrap(), value.parse().unwrap());
    }
    resp
}

async fn serve_data(
    State(shared): State<Arc<Shared>>,
    Query(q): Query<DataQuery>,
    headers: HeaderMap,
) -> Response {
    let n = shared.responses.fetch_add(1, Ordering::SeqCst);
    let refuse = |status| Response::builder().status(status).header(header::RETRY_AFTER, "1").body(Body::empty()).unwrap();
    if let Some(want) = &q.cookie
        && headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) != Some(want)
    {
        return Response::builder().status(StatusCode::FORBIDDEN).body(Body::empty()).unwrap();
    }
    if q.busy.is_some_and(|busy| n < busy) {
        return refuse(StatusCode::TOO_MANY_REQUESTS);
    }
    let already = shared.in_flight.fetch_add(1, Ordering::SeqCst);
    let guard = InFlight(shared.clone());
    if q.limit.is_some_and(|limit| already >= limit) {
        return refuse(StatusCode::SERVICE_UNAVAILABLE);
    }
    let replaced = q.after.is_some_and(|after| n >= after);
    let size = if replaced { q.then_size.or(q.size) } else { q.size };
    let size = size.unwrap_or(shared.data.len()).min(shared.data.len());
    let ranges = q.ranges != Some(0);
    let etag = if replaced { q.then_etag.as_deref().or(q.etag.as_deref()) } else { q.etag.as_deref() };
    let etag = format!("\"{}\"", etag.unwrap_or("v1"));
    let current = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()).is_none_or(|v| v == etag || v == LAST_MODIFIED);

    let mut builder = Response::builder()
        .header(header::ETAG, &etag)
        .header(header::LAST_MODIFIED, LAST_MODIFIED)
        .header(header::ACCEPT_RANGES, if ranges { "bytes" } else { "none" });
    if let Some(t) = &q.content_type {
        builder = builder.header(header::CONTENT_TYPE, t);
    }
    if let Some(name) = &q.name {
        builder = builder.header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""));
    }
    if let (Some(name), Some(value)) = (&q.header, &q.value) {
        builder = builder.header(name, value);
    }

    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let (Some(widest), Some(r)) = (q.widest, range.filter(|_| ranges))
        && !r.ends_with('-')
        && parse_range(r, usize::MAX).is_some_and(|(s, e)| e - s > widest)
    {
        return Response::builder().status(StatusCode::FORBIDDEN).body(Body::empty()).unwrap();
    }
    let (status, start, end) = match range.filter(|_| ranges && current) {
        None => (StatusCode::OK, 0, size),
        Some(r) => match parse_range(r, size) {
            Some((s, e)) => {
                builder = builder.header(header::CONTENT_RANGE, format!("bytes {s}-{}/{size}", e - 1));
                (StatusCode::PARTIAL_CONTENT, s, e)
            }
            None => {
                return builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .unwrap();
            }
        },
    };

    let cut = match (q.cut, q.cuts) {
        (Some(cut), Some(cuts)) if n < cuts => Some(cut),
        _ => None,
    };
    let crawls = start > 0 && q.crawl.is_some_and(|crawl| shared.crawled.fetch_add(1, Ordering::SeqCst) < crawl);
    let rate = if crawls { q.crawl_rate } else { q.rate };
    let mut data = shared.data.slice(start..end);
    if q.variant == Some(1) || (replaced && q.then_variant == Some(1)) {
        data = Bytes::from(variant(&data));
    }
    let body = body_stream(data, rate.filter(|&r| r > 0), cut, guard);
    builder
        .status(status)
        .header(header::CONTENT_LENGTH, end - start)
        .body(Body::from_stream(body))
        .unwrap()
        .into_response()
}

/// Parses a single `bytes=a-b` or `bytes=a-` range into a half-open `[start, end)`.
fn parse_range(value: &str, size: usize) -> Option<(usize, usize)> {
    let spec = value.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: usize = a.trim().parse().ok()?;
    let end = match b.trim() {
        "" => size,
        b => (b.parse::<usize>().ok()? + 1).min(size),
    };
    (start < end).then_some((start, end))
}

fn body_stream(
    data: Bytes,
    rate: Option<u64>,
    cut: Option<usize>,
    guard: InFlight,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    let began = Instant::now();
    futures_util::stream::unfold(0usize, move |sent| {
        let shared = guard.0.clone(); // the stream owns the guard: in flight until the body is dropped
        let data = data.clone();
        async move {
            if sent >= data.len() {
                return None;
            }
            if cut.is_some_and(|c| sent >= c) {
                let err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "cut");
                return Some((Err(err), usize::MAX));
            }
            if let Some(rate) = rate {
                let due = began + Duration::from_secs_f64(sent as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
            }
            let mut next = (sent + CHUNK).min(data.len());
            if let Some(c) = cut {
                next = next.min(c.max(sent + 1));
            }
            shared.sent.fetch_add((next - sent) as u64, Ordering::SeqCst);
            Some((Ok(data.slice(sent..next)), next))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-0", 10), Some((0, 1)));
        assert_eq!(parse_range("bytes=5-", 10), Some((5, 10)));
        assert_eq!(parse_range("bytes=5-100", 10), Some((5, 10)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("items=0-1", 10), None);
    }
}
