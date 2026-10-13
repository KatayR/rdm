use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::{CancellationToken, Error, Limiter, Options, Progress, download};
use tokio::sync::watch;

const MIB: usize = 1 << 20;

async fn server(size: usize) -> testserver::Server {
    testserver::start(([127, 0, 0, 1], 0).into(), size).await.unwrap()
}

/// An empty folder of its own for one test (so test runs at once do not meet), removed
/// when the returned guard is dropped at the end of the test.
fn dir(test: &str) -> (testserver::Folder, PathBuf) {
    let d = testserver::Folder::new(test);
    let path = d.path().to_owned();
    (d, path)
}

fn opts(dir: &Path, connections: usize) -> Options {
    Options { connections, dir: dir.to_owned(), min_split: 256 * 1024, ..Options::default() }
}

async fn get(url: &str, opts: &Options) -> engine::Result<engine::Outcome> {
    let (tx, _rx) = watch::channel(Progress::default());
    download(url, opts, &tx, &CancellationToken::new()).await
}

/// Runs a download until it has written more than `after` bytes, then stops it, which
/// saves its state for a later run to go on from. Returns the bytes it had.
async fn stop_after(url: &str, opts: &Options, after: u64) -> u64 {
    let (tx, mut rx) = watch::channel(Progress::default());
    let stop = CancellationToken::new();
    let stopping = async {
        // (`is_ok` drops the borrow `wait_for` returns; held, it would block the progress reports.)
        let seen = tokio::time::timeout(Duration::from_secs(20), rx.wait_for(|p| p.downloaded > after)).await.is_ok();
        assert!(seen, "never got to {after} bytes");
        stop.cancel();
    };
    let (ended, ()) = tokio::join!(download(url, opts, &tx, &stop), stopping);
    assert!(matches!(ended, Err(Error::Cancelled)), "not stopped part way: {:?}", ended.err());
    let had = tx.borrow().downloaded;
    assert!(had > after);
    had
}

#[tokio::test(flavor = "multi_thread")]
async fn segmented_download_matches_and_splits_dynamically() {
    let s = server(8 * MIB).await;
    let (_dir, d) = dir("segmented");
    // Three connections start with halves and quarters (1/2, 1/4, 1/4); the quarter
    // connections finish early and must take over the second half of the big one.
    let out = get(&s.url(&format!("/data?rate={}", 4 * MIB)), &opts(&d, 3)).await.unwrap();
    assert!(out.ranges);
    assert_eq!(out.path, d.join("data"));
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
    assert!(out.segments > 3, "finished connections should take over others' tails, got {} segments", out.segments);
    assert!(!d.join("data.part").exists() && !d.join("data.part.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn server_without_ranges_uses_one_connection() {
    let s = server(2 * MIB).await;
    let (_dir, d) = dir("no-ranges");
    let out = get(&s.url("/data?ranges=0"), &opts(&d, 8)).await.unwrap();
    assert!(!out.ranges);
    assert_eq!(out.segments, 1);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
    // The probe plus one download request.
    assert_eq!(s.responses(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_after_kill() {
    let s = server(8 * MIB).await;
    let (_dir, d) = dir("resume");
    let url = s.url(&format!("/data?rate={MIB}"));
    // 4 connections at 1 MiB/s need about 2 s; stop after about half.
    let killed = tokio::time::timeout(Duration::from_millis(1300), get(&url, &opts(&d, 4))).await;
    assert!(killed.is_err(), "should still be running when killed");
    assert!(d.join("data.part.json").exists());

    let out = get(&s.url("/data"), &opts(&d, 4)).await.unwrap();
    assert!(out.resumed > 2 * MIB as u64, "resumed only {} bytes", out.resumed);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_remote_file_starts_over() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("changed");
    stop_after(&s.url(&format!("/data?rate={MIB}")), &opts(&d, 2), MIB as u64 / 2).await;
    assert!(d.join("data.part.json").exists(), "nothing saved to go on from");

    // The same bytes under another ETag: only the ETag can say it is another file.
    let out = get(&s.url("/data?etag=v2"), &opts(&d, 2)).await.unwrap();
    assert_eq!(out.resumed, 0);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

/// A hash in the server's headers, on a redirect or on the file itself, is checked before the
/// file gets its name. A file that does not match stays `.part`, without state (even the
/// state of an earlier run it went on from), so the next run downloads it again.
#[tokio::test(flavor = "multi_thread")]
async fn server_hash_is_checked() {
    use sha2::Digest;
    let s = server(2 * MIB).await;
    let (_dir, d) = dir("hash-good");
    let sha256 = engine::to_hex(&sha2::Sha256::digest(&s.data));
    let out = get(&s.url(&format!("/redirect?hop_header=x-linked-etag&hop_value={sha256}")), &opts(&d, 4)).await.unwrap();
    assert_eq!(out.checked.as_deref(), Some("SHA-256 from the server (X-Linked-ETag)"));
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);

    let (_dir, d) = dir("hash-bad");
    stop_after(&s.url(&format!("/data?rate={}", MIB / 4)), &opts(&d, 4), MIB as u64 / 4).await;
    assert!(d.join("data.part.json").exists());
    let wrong = "0".repeat(40);
    let err = get(&s.url(&format!("/data?header=x-bz-content-sha1&value={wrong}")), &opts(&d, 4)).await.unwrap_err();
    assert!(matches!(err, Error::Checksum(_)), "{err:?}");
    assert!(d.join("data.part").exists() && !d.join("data.part.json").exists() && !d.join("data").exists());
    let out = get(&s.url("/data"), &opts(&d, 4)).await.unwrap();
    assert_eq!(out.resumed, 0, "nothing of the file that did not match is kept");
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);

    // No hash: nothing to check.
    let (_dir, d) = dir("hash-none");
    assert_eq!(get(&s.url("/data"), &opts(&d, 4)).await.unwrap().checked, None);
}

/// Before a resume, the end of what is on disk is fetched again: a file replaced on the
/// server by one of the same size, ETag and date (some servers keep them) starts over.
#[tokio::test(flavor = "multi_thread")]
async fn resume_starts_over_when_the_bytes_on_disk_are_outdated() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("variant");
    let killed = tokio::time::timeout(Duration::from_millis(1300), get(&s.url(&format!("/data?rate={MIB}")), &opts(&d, 2))).await;
    assert!(killed.is_err(), "should still be running when killed");
    assert!(d.join("data.part.json").exists());

    let out = get(&s.url("/data?variant=1"), &opts(&d, 2)).await.unwrap();
    assert_eq!(out.resumed, 0);
    assert!(std::fs::read(&out.path).unwrap() == testserver::variant(&s.data), "versions mixed");
}

/// An expired link given again as another address (here: another ETag for the same bytes,
/// as another server's copy has): with `new_address` the bytes on disk are kept; a file
/// that only shares the size still starts over.
#[tokio::test(flavor = "multi_thread")]
async fn resume_under_a_new_address() {
    let s = server(4 * MIB).await;
    for (test, query, want) in [("new-address", "etag=v2", s.data.to_vec()), ("new-address-other", "etag=v2&variant=1", testserver::variant(&s.data))] {
        let (_dir, d) = dir(test);
        let killed = tokio::time::timeout(Duration::from_millis(1300), get(&s.url(&format!("/data?rate={MIB}")), &opts(&d, 2))).await;
        assert!(killed.is_err(), "should still be running when killed");
        let opts = Options { new_address: true, ..opts(&d, 2) };
        let out = get(&s.url(&format!("/data?{query}")), &opts).await.unwrap();
        if test == "new-address" {
            assert!(out.resumed > MIB as u64, "{test}: resumed only {} bytes", out.resumed);
        } else {
            assert_eq!(out.resumed, 0, "{test}");
        }
        assert!(std::fs::read(&out.path).unwrap() == want, "{test}: wrong content");
    }
}

/// A file replaced on the server during the download: a range answer with another total
/// (caught without any validator), or a 200 to If-Range. Nothing is mixed: the download
/// stops, its state is gone (here the state of an earlier run that it went on from), and
/// the next run starts fresh with the new file's bytes.
#[tokio::test(flavor = "multi_thread")]
async fn file_changed_during_download_is_not_mixed() {
    for (test, change) in [("changed-size", format!("then_size={}", 5 * MIB)), ("changed-etag", "then_etag=v2".to_owned())] {
        let s = server(5 * MIB).await;
        let (_dir, d) = dir(test);
        let size = 4 * MIB;
        stop_after(&s.url(&format!("/data?size={size}&rate={MIB}")), &opts(&d, 2), MIB as u64 / 2).await;
        assert!(d.join("data.part.json").exists(), "{test}: nothing saved to go on from");

        // The next run sees the old file in its first three answers (the probe, the look at
        // the bytes on disk, a first request), then the new one; requests of 256 KiB keep
        // coming, so one of them meets it.
        let url = s.url(&format!("/data?size={size}&rate={MIB}&after={}&{change}&then_variant=1", s.responses() + 3));
        let capped = Options { max_request: Some(256 * 1024), ..opts(&d, 2) };
        let err = get(&url, &capped).await.unwrap_err();
        assert!(matches!(err, Error::Changed(_)), "{test}: {err:?}");
        assert!(d.join("data.part").exists() && !d.join("data.part.json").exists(), "{test}: state kept");

        let out = get(&url, &opts(&d, 4)).await.unwrap();
        assert_eq!(out.resumed, 0, "{test}");
        let want = testserver::variant(if test == "changed-size" { &s.data[..] } else { &s.data[..size] });
        assert!(std::fs::read(&out.path).unwrap() == want, "{test}: wrong content");
    }
}

/// A connection the server drops goes on from where it was cut. One connection, so that no
/// other can finish its work for it.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_connections_are_retried() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("retries");
    // The first four responses are cut after 300 KB: the one-byte probe (too short for
    // that), then the connection's first three requests. At 2 MiB/s, so that what was sent
    // before a cut has arrived by then.
    let out = get(&s.url(&format!("/data?cut=300000&cuts=4&rate={}", 2 * MIB)), &opts(&d, 1)).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
    assert_eq!(s.responses(), 5, "the probe, three cut answers and the one that finished");
    // Of each cut answer's 300 KB, the 256 KiB written before the cut are kept (38 KB lost,
    // three times). Starting the range over each time would cost 900 KB.
    let again = s.bytes_sent() - 4 * MIB as u64;
    assert!(again < 500_000, "{again} bytes fetched twice: the cut requests started over");
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_redirects_and_uses_server_file_name() {
    let s = server(MIB).await;
    let (_dir, d) = dir("names");
    let url = s.url("/redirect?size=1000&name=report.pdf");
    let first = get(&url, &opts(&d, 8)).await.unwrap();
    assert_eq!(first.path, d.join("report.pdf"));
    assert_eq!((first.size, first.segments), (1000, 1));
    assert_eq!(std::fs::read(&first.path).unwrap(), s.data[..1000]);

    let second = get(&url, &opts(&d, 8)).await.unwrap();
    assert_eq!(second.path, d.join("report (1).pdf"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_errors_are_reported() {
    let s = server(MIB).await;
    let (_dir, d) = dir("errors");
    let err = get(&s.url("/missing"), &opts(&d, 2)).await.unwrap_err();
    assert!(matches!(err, Error::Status(s) if s == 404), "{err:?}");

    // A refusal inside the download (403 to a range wider than 1 MiB, which all three first
    // ones are) is reported too, and not asked again: the probe, then one request each.
    let s = server(8 * MIB).await;
    let err = get(&s.url(&format!("/data?widest={MIB}")), &opts(&d, 3)).await.unwrap_err();
    assert!(matches!(err, Error::Status(s) if s == 403), "{err:?}");
    assert_eq!(s.responses(), 4, "a refused request was sent again");
}

/// A web page (HTML or XHTML) is told from a file, also a page the server sends as a file
/// to save: the browser's helper and the app's Add URL both go by this.
#[tokio::test(flavor = "multi_thread")]
async fn web_pages_are_told_from_files() {
    let s = server(MIB).await;
    let page = async |query: &str| engine::probe(&s.url(&format!("/data?size=100{query}")), &[]).await.unwrap().is_web_page();
    assert!(page("&type=text/html").await);
    assert!(page("&type=Text/HTML%3B%20charset%3Dutf-8").await, "any case, with parameters");
    assert!(page("&type=application/xhtml%2Bxml").await);
    assert!(!page("&type=text/html&name=report.html").await, "sent as a file to save");
    assert!(!page("&type=application/xhtml%2Bxml&name=report.xhtml").await, "sent as a file to save");
    assert!(!page("&type=application/zip").await);
    assert!(!page("&type=text/plain").await);
    assert!(!page("").await, "no type given");
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_limit_lowers_connection_count() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("limit");
    // At most 2 bodies at a time. The first 4 connections open together: two are turned
    // away with 503, and no more are opened after that.
    let out = get(&s.url(&format!("/data?limit=2&rate={}", 4 * MIB)), &opts(&d, 6)).await.unwrap();
    assert!((1..=2).contains(&out.refused), "{} connections stepped back", out.refused);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

/// One connection crawls while the others are fast: once they run out of pieces to split,
/// an idle one takes over the rest of the crawler's piece instead of waiting for it.
#[tokio::test(flavor = "multi_thread")]
async fn crawling_connection_is_taken_over() {
    let s = server(8 * MIB).await;
    let (_dir, d) = dir("crawl");
    let started = std::time::Instant::now();
    // At 16 KiB/s the crawler's last unsplittable piece (at least 256 KiB) alone takes 16 s.
    let out = get(&s.url("/data?crawl=1&crawl_rate=16384"), &opts(&d, 4)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "took {:?}", started.elapsed());
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

/// Every connection is slow (64 KiB/s each, as from a server that throttles connections):
/// they keep their segments to the end instead of taking them from one another, and
/// progress shows within a second. They used to look stalled until 256 KiB had arrived
/// (4 s), lost their segments at 3 s, and the download never finished.
#[tokio::test(flavor = "multi_thread")]
async fn evenly_slow_connections_finish() {
    let s = server(2 * MIB).await;
    let (_dir, d) = dir("evenly-slow");
    let (tx, mut rx) = watch::channel(Progress::default());
    let (url, o) = (s.url("/data?rate=65536"), opts(&d, 8));
    let run = tokio::spawn(async move { download(&url, &o, &tx, &CancellationToken::new()).await });
    // About a second: the first write after 1 s, reported on the next quarter second (it
    // used to take 4 s, for 256 KiB at 64 KiB/s).
    // (`is_ok` drops the borrow `wait_for` returns; held, it would block the progress reports.)
    let shown = tokio::time::timeout(Duration::from_millis(2500), rx.wait_for(|p| p.downloaded > 0)).await.is_ok();
    assert!(shown, "nothing shown after 2.5 s");
    let out = tokio::time::timeout(Duration::from_secs(30), run).await.expect("stuck near the end").unwrap().unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
    // One segment per connection at most (fewer when a connection opens late); a take-over
    // would add one.
    assert!(out.segments <= 8, "evenly slow connections took segments from one another: {}", out.segments);
}

/// A connection waiting for its share of a low speed limit is not stalled: an idle one does
/// not take its segment (which would only start it over behind the same limit).
#[tokio::test(flavor = "multi_thread")]
async fn connection_waiting_on_the_speed_limit_keeps_its_segment() {
    let s = server(MIB).await;
    let (_dir, d) = dir("held");
    // Resumed with two ranges left: 1 byte (its connection is soon idle) and 496 KiB, too
    // little to split (nothing written in either, so no bytes are read again to check the
    // file, which would take 16 s here; the rest of the file is not looked at, as the test
    // stops before the end). The server sends 16 KiB every 4 s (so the client reads 16 KiB
    // at a time, not all that piled up), and each waits about 4 s for the limit of 4 KiB/s:
    // the second connection writes only every 4 s.
    std::fs::write(d.join("data.part"), &s.data).unwrap();
    let (half, size) = (MIB as u64 / 2, MIB as u64);
    let rest = half + 16 * 1024;
    let state = format!(
        r#"{{"url":"{}","size":{size},"validator":{{"etag":"\"v1\"","last_modified":"Wed, 07 Oct 2026 12:00:00 GMT"}},"segments":[{{"start":{one},"pos":{one},"end":{half}}},{{"start":{rest},"pos":{rest},"end":{size}}}]}}"#,
        s.url("/data"),
        one = half - 1,
    );
    std::fs::write(d.join("data.part.json"), state).unwrap();
    let o = Options { limiters: vec![std::sync::Arc::new(Limiter::new(4096))], ..opts(&d, 2) };
    let (tx, _rx) = watch::channel(Progress::default());
    let stop = CancellationToken::new();
    let run = tokio::spawn({
        let (url, stop) = (s.url("/data?rate=4096"), stop.clone());
        async move { download(&url, &o, &tx, &stop).await }
    });
    tokio::time::sleep(Duration::from_secs(10)).await;
    stop.cancel();
    assert!(matches!(run.await.unwrap(), Err(Error::Cancelled)));
    let saved = std::fs::read_to_string(d.join("data.part.json")).unwrap();
    assert_eq!(saved.matches("\"start\"").count(), 2, "a segment was taken: {saved}");
    assert!(saved.contains(&format!("\"pos\":{half}")), "the first half was finished: {saved}");
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_server_is_retried_after_its_retry_after() {
    let s = server(MIB).await;
    let (_dir, d) = dir("busy");
    let started = std::time::Instant::now();
    // The first two requests (both probes) get 429 with Retry-After: 1.
    let out = get(&s.url("/data?busy=2"), &opts(&d, 4)).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1900), "waited only {:?}", started.elapsed());
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_saves_state_and_resumes() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("stop");
    let url = s.url(&format!("/data?rate={MIB}"));
    let stop = CancellationToken::new();
    let (tx, _rx) = watch::channel(Progress::default());
    let canceller = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        canceller.cancel();
    });
    let started = std::time::Instant::now();
    let err = download(&url, &opts(&d, 4), &tx, &stop).await.unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(1500), "stopping took {:?}", started.elapsed());
    // The final save on stop records everything written, not just the last periodic save.
    let saved = tx.borrow().downloaded;
    assert!(saved > MIB as u64 / 2, "only {saved} bytes before the stop");

    let out = get(&s.url("/data"), &opts(&d, 4)).await.unwrap();
    assert_eq!(out.resumed, saved);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_limiter_caps_total_speed() {
    let s = server(2 * MIB).await;
    let (_dir, d) = dir("limiter");
    let limiter = std::sync::Arc::new(Limiter::new(MIB as u64));
    let opts = Options { limiters: vec![limiter], ..opts(&d, 4) };
    let started = std::time::Instant::now();
    let out = get(&s.url("/data"), &opts).await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    // 2 MiB at 1 MiB/s over four connections: not faster than the limit (the bucket starts
    // empty; 1.6 s leaves room for timer steps), and not far slower either (2.02 to 2.74 s
    // measured, the longest right after a build; the limiter's own tests have the exact sums).
    assert!((1.6..3.5).contains(&secs), "took {secs:.2} s");
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);

    // With several limits (as for all downloads and for this one), the lowest one holds,
    // whichever comes first.
    for rates in [[2 * MIB, MIB], [MIB, 2 * MIB]] {
        let (_dir, d) = dir("limiters");
        let limiters = rates.iter().map(|&rate| std::sync::Arc::new(Limiter::new(rate as u64))).collect();
        let started = std::time::Instant::now();
        get(&s.url("/data"), &Options { limiters, ..self::opts(&d, 4) }).await.unwrap();
        let secs = started.elapsed().as_secs_f64();
        assert!(secs >= 1.6, "limits of {rates:?} bytes/s: 2 MiB took {secs:.2} s");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn capped_requests_pass_a_server_that_refuses_wide_ranges() {
    let s = server(8 * MIB).await;
    let (_dir, d) = dir("capped");
    let url = s.url(&format!("/data?widest={MIB}&rate={}", 4 * MIB));
    let err = get(&url, &opts(&d, 3)).await.unwrap_err();
    assert!(matches!(err, Error::Status(st) if st == 403), "{err:?}");

    // Same uneven start as the segmented test, so splitting still happens under the cap.
    let opts = Options { max_request: Some(MIB as u64), ..opts(&d, 3) };
    let out = get(&url, &opts).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
    assert!(out.segments > 3, "got {} segments", out.segments);
    // The probe, the failed run's requests, and at least one request per MiB.
    assert!(s.responses() >= 8, "only {} responses", s.responses());
}

#[tokio::test(flavor = "multi_thread")]
async fn capped_download_resumes_after_stop() {
    let s = server(4 * MIB).await;
    let (_dir, d) = dir("capped-stop");
    let url = s.url(&format!("/data?widest={}&rate={MIB}", 512 * 1024));
    let opts = Options { max_request: Some(512 * 1024), ..opts(&d, 2) };
    let killed = tokio::time::timeout(Duration::from_millis(1300), get(&url, &opts)).await;
    assert!(killed.is_err(), "should still be running when killed");

    let out = get(&s.url("/data?widest=524288"), &opts).await.unwrap();
    assert!(out.resumed > MIB as u64, "resumed only {} bytes", out.resumed);
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn request_headers_are_sent() {
    let s = server(MIB).await;
    let (_dir, d) = dir("headers");
    let url = s.url("/redirect?cookie=session%3D42");
    let err = get(&url, &opts(&d, 4)).await.unwrap_err();
    assert!(matches!(err, Error::Status(st) if st == 403), "{err:?}");

    let opts = Options { headers: vec![("Cookie".into(), "session=42".into())], ..opts(&d, 4) };
    let out = get(&url, &opts).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data);
}
