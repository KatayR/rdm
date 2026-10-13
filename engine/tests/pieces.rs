//! Streams made of pieces (HLS) against the test server's playlists.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::hls;
use engine::{CancellationToken, Error, Limiter, Options, Progress, download_pieces};
use tokio::sync::watch;

async fn server(size: usize) -> testserver::Server {
    testserver::start(([127, 0, 0, 1], 0).into(), size).await.unwrap()
}

/// An empty folder of its own for one test (so test runs at once do not meet), removed
/// when the returned guard is dropped at the end of the test.
fn dir(test: &str) -> (testserver::Folder, PathBuf) {
    let d = testserver::Folder::new(&format!("pieces-{test}"));
    let path = d.path().to_owned();
    (d, path)
}

fn opts(dir: &Path, connections: usize) -> Options {
    Options { connections, dir: dir.to_owned(), file_name: Some("video.ts".into()), ..Options::default() }
}

async fn get(s: &testserver::Server, query: &str, opts: &Options) -> engine::Result<engine::Outcome> {
    let list = hls::load(&s.url(&format!("/hls/index.m3u8?{query}")), &[]).await?;
    let (tx, _rx) = watch::channel(Progress::default());
    download_pieces(list, opts, &tx, &CancellationToken::new()).await
}

/// The stream as the test server builds it, pieces `0..pieces` of `size` bytes.
fn expected(s: &testserver::Server, pieces: usize, size: usize, ts: bool) -> Vec<u8> {
    (0..pieces).flat_map(|n| s.piece(n, size, ts)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pieces_are_joined_in_order() {
    let s = server(1 << 20).await;
    let (_dir, d) = dir("plain");
    let out = get(&s, "pieces=40&size=5000", &opts(&d, 8)).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 40, 5000, false));
    assert_eq!((out.path, out.segments), (d.join("video.ts"), 40));
    assert!(!d.join("video.ts.part").exists() && !d.join("video.ts.part.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn aes_128_pieces_are_decrypted() {
    let s = server(1 << 20).await;
    for key in ["seq", "iv"] {
        let (_dir, d) = dir(&format!("aes-{key}"));
        let out = get(&s, &format!("pieces=6&size=3001&key={key}"), &opts(&d, 3)).await.unwrap();
        assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 6, 3001, false), "IV from {key}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn byte_ranges_with_an_init_section() {
    let s = server(1 << 20).await;
    let (_dir, d) = dir("ranges");
    let out = get(&s, "pieces=5&size=7000&map=1", &opts(&d, 4)).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), s.data[..6 * 7000].to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn disguised_pieces_lose_their_fake_header() {
    let s = server(1 << 20).await;
    let (_dir, d) = dir("disguise");
    let out = get(&s, "pieces=4&size=4000&disguise=1", &opts(&d, 2)).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 4, 4000, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_piece_fails_and_keeps_what_was_written() {
    let s = server(1 << 20).await;
    let (_dir, d) = dir("fail");
    let err = get(&s, "pieces=6&size=1000&fail=3", &opts(&d, 1)).await.unwrap_err();
    assert!(matches!(err, Error::Status(st) if st == 404), "{err}");
    assert_eq!(s.pieces_served(), 4, "pieces 0 to 3, the missing one asked for once");
    assert!(d.join("video.ts.part.json").exists(), "progress kept for a later resume");
    assert!(!d.join("video.ts").exists());

    // The same stream with the piece there: the three written pieces are kept.
    let out = get(&s, "pieces=6&size=1000", &opts(&d, 1)).await.unwrap();
    assert_eq!(out.resumed, 3000);
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 6, 1000, false));
}

/// A list counted from a video's length (DASH) may name one piece too many: without the last
/// one the stream is whole. Any other missing piece still fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_counted_list_may_end_one_piece_early() {
    let s = server(1 << 20).await;
    let list = |missing: usize| {
        let pieces = (0..5).map(|n| (s.url(&format!("/hls/piece?n={n}&size=1000&fail={missing}")), None)).collect();
        engine::Pieces::new(&s.url("/manifest.mpd"), &[], pieces).unwrap()
    };
    let run = async |missing: usize, d: &Path| {
        let (tx, _rx) = watch::channel(Progress::default());
        engine::download_pieces(list(missing), &opts(d, 2), &tx, &CancellationToken::new()).await
    };
    let (_dir, d) = dir("open-end");
    let out = run(4, &d).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 4, 1000, false));

    let (_dir, d) = dir("open-end-middle");
    let err = run(3, &d).await.unwrap_err();
    assert!(matches!(err, Error::Status(st) if st == 404), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn live_playlists_are_refused() {
    let s = server(1 << 20).await;
    let err = hls::load(&s.url("/hls/index.m3u8?live=1"), &[]).await.unwrap_err();
    assert!(matches!(err, Error::Live), "{err}");
}

/// Downloads 20 pieces of 64 KiB at 128 KiB/s each over two connections (about 5 s in all)
/// and stops after 1.5 s.
async fn stop_part_way(s: &testserver::Server, d: &Path) -> engine::Pieces {
    let list = hls::load(&s.url("/hls/index.m3u8?pieces=20&size=65536&rate=131072"), &[]).await.unwrap();
    let (tx, rx) = watch::channel(Progress::default());
    let stop = CancellationToken::new();
    let stopper = {
        let stop = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            stop.cancel();
        })
    };
    let err = download_pieces(list.clone(), &opts(d, 2), &tx, &stop).await.unwrap_err();
    stopper.await.unwrap();
    assert!(matches!(err, Error::Cancelled));
    let first = rx.borrow().downloaded;
    assert!(first > 0 && first < 20 * 65536, "stopped part way: {first}");
    // Whole pieces on disk, as the saved state counts them (the progress above also counts
    // what open requests had received).
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("video.ts.part.json")).unwrap()).unwrap();
    assert!(state["written"].as_u64().unwrap() > 0, "no piece written before the stop: {state}");
    list
}

#[tokio::test(flavor = "multi_thread")]
async fn stopped_download_resumes_with_the_pieces_it_had() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("resume");
    let list = stop_part_way(&s, &d).await;

    let (tx, rx) = watch::channel(Progress::default());
    let out = download_pieces(list, &opts(&d, 2), &tx, &CancellationToken::new()).await.unwrap();
    assert!(out.resumed > 0, "pieces of the first run kept");
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 20, 65536, false));
    assert_eq!(rx.borrow().total, Some(20 * 65536));
    assert_eq!(out.resumed % 65536, 0, "resume starts at a piece boundary");
}

/// A list of the same length whose pieces now hold other bytes (a new encode behind the
/// same addresses): the last piece on disk no longer matches, so it starts over.
#[tokio::test(flavor = "multi_thread")]
async fn resume_starts_over_when_the_last_piece_differs() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("variant");
    stop_part_way(&s, &d).await;

    let list = hls::load(&s.url("/hls/index.m3u8?pieces=20&size=65536&variant=1"), &[]).await.unwrap();
    let (tx, _rx) = watch::channel(Progress::default());
    let out = download_pieces(list, &opts(&d, 2), &tx, &CancellationToken::new()).await.unwrap();
    assert_eq!(out.resumed, 0);
    assert!(std::fs::read(&out.path).unwrap() == testserver::variant(&expected(&s, 20, 65536, false)), "versions mixed");
}

/// One piece is slow (4 KiB/s the first time: 16 s for its 64 KiB): the other connections
/// fetch ahead meanwhile, and the slow piece is fetched again on a free one, so the whole
/// stream takes a few seconds, in the right order.
#[tokio::test(flavor = "multi_thread")]
async fn slow_piece_is_taken_over() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("slow");
    let started = std::time::Instant::now();
    let out = get(&s, "pieces=16&size=65536&slow=2&slow_rate=4096", &opts(&d, 4)).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "took {:?}", started.elapsed());
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 16, 65536, false));
}

/// Pieces about as slow as one another are not fetched twice: the head runs at 3/4 of the
/// speed of the piece that overtakes it (as evenly slow pieces finish out of order by
/// chance), not half, so it keeps its one request.
#[tokio::test(flavor = "multi_thread")]
async fn evenly_slow_pieces_are_fetched_once() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("even");
    // 256 KiB pieces at 64 KiB/s (4 s), the first at 48 KiB/s (5.3 s).
    let out = get(&s, "pieces=2&size=262144&rate=65536&slow=0&slow_rate=49152", &opts(&d, 2)).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 2, 262144, false));
    assert_eq!(s.pieces_served(), 2, "a piece was fetched twice");
}

/// A head piece waiting on the speed limit is not fetched again (the limit holds it back;
/// a second request would only share it), though it is quiet and a connection is free: two
/// 16 KiB pieces (one chunk each) behind 4 KiB/s on three connections, so the head waits 4
/// or 8 s for the limit, whichever piece is charged first, with the third connection idle.
#[tokio::test(flavor = "multi_thread")]
async fn piece_waiting_on_the_speed_limit_is_fetched_once() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("held");
    let o = Options { limiters: vec![std::sync::Arc::new(Limiter::new(4096))], ..opts(&d, 3) };
    let out = get(&s, "pieces=2&size=16384", &o).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), expected(&s, 2, 16384, false));
    assert_eq!(s.pieces_served(), 2, "a piece was fetched twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn progress_counts_pieces_and_open_requests() {
    let s = server(4 << 20).await;
    let (_dir, d) = dir("progress");
    // 12 pieces of 64 KiB at 256 KiB/s each over 3 connections: about 1 s.
    let list = hls::load(&s.url("/hls/index.m3u8?pieces=12&size=65536&rate=262144"), &[]).await.unwrap();
    let (tx, mut rx) = watch::channel(Progress::default());
    let seen = tokio::spawn(async move {
        let mut seen = Vec::new();
        while rx.changed().await.is_ok() {
            seen.push(rx.borrow().clone());
        }
        seen
    });
    download_pieces(list, &opts(&d, 3), &tx, &CancellationToken::new()).await.unwrap();
    drop(tx);
    let seen = seen.await.unwrap();
    let (running, last) = seen.split_at(seen.len() - 1);
    assert!(!running.is_empty(), "reported while running");
    for p in running {
        let (written, all) = p.pieces.expect("piece counts");
        assert_eq!(all, 12);
        assert!(p.connections <= 3, "open requests, at most the connections: {}", p.connections);
        // No byte ranges: the size is measured on the pieces so far.
        assert_eq!(p.estimated, written < 12);
    }
    assert!(running.iter().any(|p| p.connections > 0), "requests counted while open");
    let last = &last[0];
    assert_eq!((last.pieces, last.estimated, last.connections), (Some((12, 12)), false, 0));
    assert_eq!(last.total, Some(12 * 65536));

    // Byte ranges give the exact size from the start: in every report, not only the last
    // (6 requests of 64 KiB at 128 KiB/s over 2 connections: about 1 s, so with reports
    // while some pieces are written and others not).
    let (_dir, d) = dir("progress-ranges");
    let list = hls::load(&s.url("/hls/index.m3u8?pieces=5&size=65536&map=1&rate=131072"), &[]).await.unwrap();
    let (tx, mut rx) = watch::channel(Progress::default());
    let seen = tokio::spawn(async move {
        let mut seen = Vec::new();
        while rx.changed().await.is_ok() {
            seen.push(rx.borrow().clone());
        }
        seen
    });
    download_pieces(list, &opts(&d, 2), &tx, &CancellationToken::new()).await.unwrap();
    drop(tx);
    let seen = seen.await.unwrap();
    assert!(seen.iter().any(|p| p.pieces.is_some_and(|(written, _)| 0 < written && written < 6)), "reported while running");
    for p in &seen {
        assert_eq!((p.estimated, p.total), (false, Some(6 * 65536)), "{:?} pieces", p.pieces);
    }
    assert_eq!(seen.last().unwrap().pieces, Some((6, 6)));
}
