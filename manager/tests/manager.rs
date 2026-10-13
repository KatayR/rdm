use std::path::PathBuf;
use std::time::{Duration, Instant};

use manager::model::{Download, Event, NewDownload, Status};
use manager::{Config, MAIN_QUEUE, Manager};

const MIB: usize = 1 << 20;

async fn server(size: usize) -> testserver::Server {
    testserver::start(([127, 0, 0, 1], 0).into(), size).await.unwrap()
}

/// Data and downloads folders of its own for one test, in a folder that goes when the guard
/// (first, so dropped after the manager) is dropped.
fn config(test: &str) -> (testserver::Folder, Config, PathBuf) {
    let dir = testserver::Folder::new(&format!("manager-{test}"));
    let downloads = dir.path().join("downloads");
    let config = Config { downloads_dir: Some(downloads.clone()), min_split: 256 * 1024, ..Config::new(dir.path().join("home")) };
    (dir, config, downloads)
}

fn new(url: String, start: bool) -> NewDownload {
    NewDownload { url, start, ..NewDownload::default() }
}

async fn wait_for(m: &Manager, what: &str, done: impl Fn(&[Download]) -> bool) -> Vec<Download> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list = m.list();
        if done(&list) {
            return list;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {list:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn all_complete(list: &[Download]) -> bool {
    list.iter().all(|d| d.status == Status::Complete)
}

/// A manager on the data folder of an earlier one, as after a restart.
fn reopen(home: PathBuf) -> Manager {
    Manager::open(Config { min_split: 256 * 1024, ..Config::new(home) }).unwrap()
}

/// The most the server sends for a download that goes on from `downloaded` of `size` bytes:
/// the rest, plus room for the bytes fetched again to compare (64 KiB) and for what was on
/// its way when it stopped. Starting over costs `downloaded` more, so the tests stop a
/// download only when it has well over this room.
fn rest(size: usize, downloaded: u64) -> u64 {
    size as u64 - downloaded + ROOM
}

const ROOM: u64 = 512 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn queue_keeps_order_and_parallel_limit() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("queue");
    let m = Manager::open(config).unwrap();
    let ids: Vec<i64> =
        (0..4).map(|i| m.add(new(s.url(&format!("/data?name=f{i}.bin&rate={}", 4 * MIB)), false)).unwrap().id).collect();
    // The first goes to the end: the order to keep is the queue's, not the one they were
    // added in.
    m.move_to_queue(&ids[..1], MAIN_QUEUE).unwrap();
    let mut main = m.queues().into_iter().find(|q| q.id == MAIN_QUEUE).unwrap();
    main.parallel = 2;
    m.update_queue(main).unwrap();
    assert!(m.list().iter().all(|d| d.status == Status::Waiting), "nothing starts before the queue");

    let mut events = m.subscribe();
    m.start_queue(MAIN_QUEUE).unwrap();
    let mut most = 0;
    let deadline = Instant::now() + Duration::from_secs(20);
    let list = loop {
        let list = m.list();
        most = most.max(list.iter().filter(|d| d.status == Status::Downloading).count());
        if all_complete(&list) {
            break list;
        }
        assert!(Instant::now() < deadline, "the queue did not finish: {list:#?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(most, 2);
    let started: Vec<i64> =
        std::iter::from_fn(|| events.try_recv().ok()).filter_map(|e| if let Event::Started(id) = e { Some(id) } else { None }).collect();
    assert_eq!(started, [ids[1], ids[2], ids[3], ids[0]], "started in the queue's order");
    for d in &list {
        assert_eq!(std::fs::read(d.path.as_ref().unwrap()).unwrap(), s.data);
    }
    assert!(!m.queues()[0].running, "a queue with nothing left stops");
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn events_follow_a_download() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("events");
    let m = Manager::open(config).unwrap();
    let mut events = m.subscribe();
    let id = m.add(new(s.url("/data"), true)).unwrap().id;
    let next = async { tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap() };
    assert_eq!(next.await, Event::Started(id));
    let next = async { tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap() };
    assert_eq!(next.await, Event::Complete(id));
    m.shutdown().await;
}

/// A window toolkit calls in from its main thread, which has no Tokio runtime; starting a
/// download there used to panic (rdm-app crashed on Start Download).
#[tokio::test(flavor = "multi_thread")]
async fn calls_from_a_thread_without_a_runtime_work() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("no-runtime");
    let m = Manager::open(config).unwrap();
    let (m2, url) = (m.clone(), s.url("/data"));
    let id = std::thread::spawn(move || m2.add(new(url, true)).unwrap().id).join().expect("no panic");
    // One that waits in the queue, so that starting the queue from there starts a download.
    let (m2, url) = (m.clone(), s.url("/data?name=queued.bin"));
    let start_queue = move || {
        let queued = m2.add(new(url, false)).unwrap().id;
        m2.start_queue(MAIN_QUEUE).unwrap();
        queued
    };
    let queued = std::thread::spawn(start_queue).join().expect("no panic");
    let list = wait_for(&m, "completion", all_complete).await;
    assert_eq!([list[0].id, list[1].id], [id, queued]);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_resume() {
    let s = server(4 * MIB).await;
    let (_dir, config, _) = config("pause");
    let m = Manager::open(config).unwrap();
    let id = m.add(new(s.url(&format!("/data?rate={}", MIB / 8)), true)).unwrap().id;
    wait_for(&m, "progress", |l| l[0].downloaded > MIB as u64).await;
    m.pause(id).await.unwrap();
    let paused = m.get(id).unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert!(paused.downloaded > 2 * ROOM, "{} bytes", paused.downloaded);
    let sent = s.bytes_sent();

    m.resume(id).unwrap();
    let list = wait_for(&m, "completion", all_complete).await;
    assert_eq!(std::fs::read(list[0].path.as_ref().unwrap()).unwrap(), s.data);
    let more = s.bytes_sent() - sent;
    assert!(more <= rest(4 * MIB, paused.downloaded), "started over: {more} bytes fetched after {}", paused.downloaded);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_keeps_the_list_and_resumes() {
    let s = server(4 * MIB).await;
    let (_dir, config, _) = config("restart");
    let home = config.home.clone();
    let m = Manager::open(config).unwrap();
    let id = m.add(new(s.url(&format!("/data?rate={}", MIB / 8)), true)).unwrap().id;
    m.add(new(s.url("/data?name=later.bin"), false)).unwrap();
    wait_for(&m, "progress", |l| l[0].downloaded > MIB as u64).await;
    m.shutdown().await;
    let before = m.get(id).unwrap();
    assert_eq!(before.status, Status::Downloading, "stays marked for the next start");
    assert!(before.downloaded > 2 * ROOM, "{} bytes", before.downloaded);
    drop(m);
    let sent = s.bytes_sent();

    let m = reopen(home);
    let list = m.list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[1].status, Status::Waiting);
    assert_eq!(list[0].downloaded, before.downloaded, "progress saved on shutdown");
    let list = wait_for(&m, "resumed completion", |l| l[0].status == Status::Complete).await;
    assert_eq!(std::fs::read(list[0].path.as_ref().unwrap()).unwrap(), s.data);
    let more = s.bytes_sent() - sent;
    assert!(more <= rest(4 * MIB, before.downloaded), "started over: {more} bytes fetched after {}", before.downloaded);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_replaced_on_the_server_starts_over() {
    let s = server(2 * MIB).await;
    let (_dir, config, _) = config("replaced");
    let m = Manager::open(config).unwrap();
    // From the fourth answer on, the server has a bigger file: the first run stops at
    // once, and the download starts over by itself.
    m.add(new(s.url(&format!("/data?size={MIB}&after=3&then_size={}&rate={MIB}", 2 * MIB)), true)).unwrap();
    let list = wait_for(&m, "the end", |l| matches!(l[0].status, Status::Complete | Status::Failed)).await;
    assert_eq!(list[0].status, Status::Complete, "{:?}", list[0].error);
    assert_eq!(list[0].size, Some(2 * MIB as u64));
    assert_eq!(std::fs::read(list[0].path.as_ref().unwrap()).unwrap(), s.data);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_hash_is_recorded_or_fails_the_download() {
    let s = server(MIB).await;
    let (_dir, config, downloads) = config("hash");
    let m = Manager::open(config).unwrap();
    let sha1 = hash_hex(&s.data, engine::HashKind::Sha1);
    // x-goog-hash: md5= the base64 of 16 bytes of 0xff, which is not the data's MD5.
    let wrong = "md5%3D%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2F%2Fw%3D%3D";
    m.add(new(s.url(&format!("/data?name=good.bin&header=x-bz-content-sha1&value={sha1}")), true)).unwrap();
    m.add(new(s.url(&format!("/data?name=bad.bin&header=x-goog-hash&value={wrong}")), true)).unwrap();
    let list = wait_for(&m, "both to end", |l| l.iter().all(|d| matches!(d.status, Status::Complete | Status::Failed))).await;
    assert_eq!(list[0].status, Status::Complete, "{:?}", list[0].error);
    assert_eq!(list[0].checked.as_deref(), Some("SHA-1 from the server (X-Bz-Content-Sha1)"));
    assert_eq!(list[1].status, Status::Failed);
    assert!(list[1].error.as_deref().unwrap().contains("does not match the MD5"), "{:?}", list[1].error);
    assert!(downloads.join("bad.bin.part").exists() && !downloads.join("bad.bin").exists());

    // The complete window's check: the SHA-256 when no hash is given, else a comparison.
    let id = list[0].id;
    let check = m.check_hash(id, "").await.unwrap();
    assert_eq!((check.kind.as_str(), check.matches), ("SHA-256", None));
    assert_eq!(check.hash, hash_hex(&s.data, engine::HashKind::Sha256));
    assert_eq!(m.check_hash(id, &sha1.to_uppercase()).await.unwrap().matches, Some(true));
    assert_eq!(m.check_hash(id, &"0".repeat(40)).await.unwrap().matches, Some(false));
    assert!(m.check_hash(list[1].id, "").await.is_err(), "not complete");
    m.shutdown().await;
}

fn hash_hex(data: &[u8], kind: engine::HashKind) -> String {
    let dir = testserver::Folder::new("hash");
    let path = dir.path().join("data.bin");
    std::fs::write(&path, data).unwrap();
    engine::to_hex(&engine::hash_file(&path, kind).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_zip_files_are_tested() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("filecheck");
    let m = Manager::open(config).unwrap();
    // The test server's bytes are no zip archive: complete all the same, with a warning.
    m.add(new(s.url("/data?name=pack.zip"), true)).unwrap();
    m.add(new(s.url("/data?name=data.bin"), true)).unwrap();
    let list = wait_for(&m, "the checks", |l| all_complete(l) && l.iter().all(|d| !d.checking)).await;
    let check = list[0].file_check.as_ref().unwrap();
    assert!(!check.ok && check.text.starts_with("not a readable zip archive"), "{check:?}");
    assert_eq!(list[1].file_check, None, "no check for other types");
    m.shutdown().await;
}

/// Two video downloads of the same name: removing one deletes its stream parts (finished
/// or not) and nothing of the other's, an older part or the joined file.
#[tokio::test(flavor = "multi_thread")]
async fn remove_deletes_a_videos_own_stream_parts() {
    let (_dir, config, downloads) = config("stream-parts");
    let m = Manager::open(config).unwrap();
    std::fs::create_dir_all(&downloads).unwrap();
    let video = || NewDownload {
        url: "https://example.org/watch?v=1".into(),
        video: Some("best".into()),
        dir: Some(downloads.clone()),
        file_name: Some("clip.mp4".into()),
        ..NewDownload::default()
    };
    let a = m.add(video()).unwrap().id;
    let b = m.add(video()).unwrap().id;
    let mine = [format!("clip.d{a}.f137.mp4"), format!("clip.d{a}.f140.m4a.part"), format!("clip.d{a}.f140.m4a.part.json")];
    let others = [format!("clip.d{b}.f137.mp4"), "clip.f137.mp4".into(), "clip.mp4".into()];
    for f in mine.iter().chain(&others) {
        std::fs::write(downloads.join(f), f).unwrap();
    }
    m.remove(a, false).await.unwrap();
    let mut left: Vec<String> = std::fs::read_dir(&downloads).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    let mut want = others.to_vec();
    want.sort();
    assert_eq!(left, want);
    m.shutdown().await;
}

/// A link already in the list is found by its address (an unanswered one does not count),
/// and a new download of a file with the same name in the same folder takes a free name, so
/// the paused one's part file is left alone.
#[tokio::test(flavor = "multi_thread")]
async fn same_address_found_and_same_name_kept_apart() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("same");
    let m = Manager::open(config).unwrap();
    let url = s.url(&format!("/data?name=same.bin&rate={}", MIB / 4));
    let first = m.add(new(url.clone(), true)).unwrap().id;
    wait_for(&m, "progress", |l| l[0].downloaded > MIB as u64 / 2).await;
    m.pause(first).await.unwrap();
    assert_eq!(m.find_same_url(&url).map(|d| d.id), Some(first));
    assert!(m.find_same_url(&s.url("/data?name=other.bin")).is_none());
    let state = std::fs::read(downloads.join("same.bin.part.json")).unwrap();

    let other = s.url("/data?name=same.bin&etag=v2");
    let early = m.add(NewDownload { url: other.clone(), provisional: true, ..NewDownload::default() }).unwrap().id;
    let deadline = Instant::now() + Duration::from_secs(10);
    while m.get(early).unwrap().resumable.is_none() {
        assert!(Instant::now() < deadline, "no answer from the server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(m.get(early).unwrap().file_name.as_deref(), Some("same (1).bin"));
    assert!(m.find_same_url(&other).is_none(), "an unanswered download does not count");
    assert_eq!(std::fs::read(downloads.join("same.bin.part.json")).unwrap(), state, "the paused one untouched");
    m.remove(early, true).await.unwrap();

    m.resume(first).unwrap();
    let list = wait_for(&m, "completion", all_complete).await;
    assert_eq!(list[0].path.as_deref(), Some(downloads.join("same.bin").as_path()));
    assert_eq!(std::fs::read(downloads.join("same.bin")).unwrap(), s.data);
    m.shutdown().await;
}

/// A complete download and a video have no `<name>.part` of their own: the one in the
/// folder is another download's of the same name (the file downloaded again, or a plain
/// file next to the video). Removing them leaves it and its progress alone.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_download_leaves_anothers_part_file() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("anothers-part");
    let m = Manager::open(config).unwrap();
    let complete = m.add(new(s.url("/data?name=same.bin&size=100000"), true)).unwrap().id;
    wait_for(&m, "completion", all_complete).await;
    // The same file again: a quarter minute on one connection, placed under the same name.
    let again = NewDownload { connections: Some(1), ..new(s.url(&format!("/data?name=same.bin&rate={}", MIB / 4)), true) };
    let again = m.add(again).unwrap().id;
    wait_for(&m, "progress", |l| l[1].downloaded > 0).await;
    m.pause(again).await.unwrap();
    assert_eq!(m.get(again).unwrap().file_name.as_deref(), Some("same.bin"));
    let state = std::fs::read(downloads.join("same.bin.part.json")).unwrap();
    let video = NewDownload {
        url: "https://example.org/watch?v=1".into(),
        video: Some("best".into()),
        dir: Some(downloads.clone()),
        file_name: Some("same.bin".into()),
        ..NewDownload::default()
    };
    let video = m.add(video).unwrap().id;

    for (what, id) in [("the complete one", complete), ("the video", video)] {
        m.remove(id, false).await.unwrap();
        assert!(downloads.join("same.bin.part").is_file(), "the part file, after removing {what}");
        assert_eq!(std::fs::read(downloads.join("same.bin.part.json")).unwrap(), state, "the progress, after removing {what}");
    }
    assert_eq!(std::fs::read(downloads.join("same.bin")).unwrap(), s.data[..100_000], "removed from the list only");
    // Its own go with it.
    m.remove(again, false).await.unwrap();
    assert!(!downloads.join("same.bin.part").exists() && !downloads.join("same.bin.part.json").exists());
    m.shutdown().await;
}

/// A download added with a folder and a name (Download File Info's answer) only asks for the
/// name until its server answers. The unfinished download that has the name keeps it, and
/// its part file, whether the other one waits, is removed, or starts and takes " (1)".
#[tokio::test(flavor = "multi_thread")]
async fn a_download_not_yet_placed_holds_no_name() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("asked-name");
    let m = Manager::open(config).unwrap();
    let files = || {
        let mut files: Vec<String> = std::fs::read_dir(&downloads).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        files
    };
    let slow = |query: &str| NewDownload { connections: Some(1), ..new(s.url(&format!("/data?name=same.bin&rate={}{query}", MIB / 4)), false) };
    let original = m.add(NewDownload { start: true, ..slow("") }).unwrap().id;
    wait_for(&m, "progress", |l| l[0].downloaded > 0).await;
    m.pause(original).await.unwrap();
    let asking = || NewDownload { dir: Some(downloads.clone()), file_name: Some("same.bin".into()), ..slow("&again=1") };

    // Resumed beside one that waits: under its own name, in its own part file.
    let waiting = m.add(asking()).unwrap().id;
    let before = m.get(original).unwrap().downloaded;
    m.resume(original).unwrap();
    wait_for(&m, "more progress", |l| l[0].downloaded > before).await;
    m.pause(original).await.unwrap();
    assert_eq!(m.get(original).unwrap().file_name.as_deref(), Some("same.bin"));
    assert_eq!(files(), ["same.bin.part", "same.bin.part.json"]);
    let state = std::fs::read(downloads.join("same.bin.part.json")).unwrap();

    // The waiting one removed: nothing of the original goes.
    m.remove(waiting, true).await.unwrap();
    assert_eq!(files(), ["same.bin.part", "same.bin.part.json"]);
    assert_eq!(std::fs::read(downloads.join("same.bin.part.json")).unwrap(), state);

    // Started, it is given a name of its own, and then its part file goes with it.
    let started = m.add(NewDownload { start: true, ..asking() }).unwrap().id;
    wait_for(&m, "progress of the second", |l| l[1].downloaded > 0).await;
    assert_eq!(m.get(started).unwrap().file_name.as_deref(), Some("same (1).bin"));
    m.remove(started, true).await.unwrap();
    assert_eq!(files(), ["same.bin.part", "same.bin.part.json"]);
    assert_eq!(std::fs::read(downloads.join("same.bin.part.json")).unwrap(), state);
    m.shutdown().await;
}

/// An unfinished download whose link expired, found again by the browser's fresh link (same
/// name, maybe with " (n)", and size), continues from the new address without starting over.
#[tokio::test(flavor = "multi_thread")]
async fn expired_link_continued_from_a_new_address() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("refresh");
    let m = Manager::open(config).unwrap();
    let id = m.add(new(s.url(&format!("/data?name=movie.bin&rate={}", MIB / 4)), true)).unwrap().id;
    wait_for(&m, "progress", |l| l[0].downloaded > MIB as u64).await;
    m.pause(id).await.unwrap();
    let before = m.get(id).unwrap().downloaded;
    assert!(before > 2 * ROOM, "{before} bytes");
    let sent = s.bytes_sent();

    assert_eq!(m.find_same_file("movie (1).bin", 4 * MIB as u64).map(|d| d.id), Some(id));
    assert!(m.find_same_file("movie.bin", 1000).is_none(), "another size");
    assert!(m.find_same_file("other.bin", 4 * MIB as u64).is_none(), "another name");

    // The fresh link: another address with another ETag (another server's copy), same bytes.
    let fresh = s.url("/data?name=movie.bin&etag=v2");
    m.refresh_address(id, fresh.clone(), vec![("Cookie".into(), "a=b".into())]).await.unwrap();
    let list = wait_for(&m, "the end", |l| l[0].status != Status::Downloading).await;
    let d = &list[0];
    assert_eq!(d.status, Status::Complete, "{:?}", d.error);
    let more = s.bytes_sent() - sent;
    assert!(more <= rest(4 * MIB, before), "started over: {more} bytes fetched after {before}");
    assert_eq!((d.url.as_str(), d.headers.len()), (fresh.as_str(), 1));
    assert_eq!(std::fs::read(downloads.join("movie.bin")).unwrap(), s.data);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn description_set_and_cleared() {
    let (_dir, config, _) = config("description");
    let home = config.home.clone();
    let m = Manager::open(config).unwrap();
    let id = m.add(new("https://example.org/a.zip".into(), false)).unwrap().id;
    m.set_description(id, Some("for the trip".into())).unwrap();
    assert_eq!(m.get(id).unwrap().description.as_deref(), Some("for the trip"));
    m.shutdown().await;
    drop(m);
    let m = reopen(home.clone());
    assert_eq!(m.get(id).unwrap().description.as_deref(), Some("for the trip"), "kept over a restart");

    m.set_description(id, Some("   ".into())).unwrap();
    assert_eq!(m.get(id).unwrap().description, None, "blank clears it");
    assert!(m.set_description(9999, None).is_err());
    m.shutdown().await;
    drop(m);
    let m = reopen(home);
    assert_eq!(m.get(id).unwrap().description, None, "cleared over a restart");
    m.shutdown().await;
}

/// Moved downloads go to the end of the other queue, in the order given.
#[tokio::test(flavor = "multi_thread")]
async fn moved_downloads_go_to_the_end_of_the_queue() {
    let (_dir, config, _) = config("move-to-queue");
    let m = Manager::open(config).unwrap();
    let later = m.new_queue("Later").unwrap().id;
    let add = |name: &str, queue| m.add(NewDownload { url: format!("https://example.org/{name}"), queue, ..NewDownload::default() }).unwrap().id;
    let (a, b, c) = (add("a.zip", None), add("b.zip", None), add("c.zip", Some(later)));
    let place = |id| {
        let d = m.get(id).unwrap();
        (d.queue, d.position)
    };
    m.move_to_queue(&[b, a], later).unwrap();
    assert_eq!([place(c), place(b), place(a)], [(later, 1), (later, 2), (later, 3)]);
    // Into the emptied queue from 1; an unknown id stops there, the ones before it moved.
    assert!(m.move_to_queue(&[c, 9999, a], MAIN_QUEUE).is_err());
    assert_eq!([place(c), place(a)], [(MAIN_QUEUE, 1), (later, 3)]);
    assert!(m.move_to_queue(&[a], 9999).is_err(), "no such queue");
    m.shutdown().await;
}

/// A download started while Download File Info is still open (as IDM does).
fn early(url: String) -> NewDownload {
    NewDownload { url, provisional: true, ..NewDownload::default() }
}

/// What the person has not answered yet stays out of the list and sends no events; the
/// answer can move it to another folder and name while it runs.
#[tokio::test(flavor = "multi_thread")]
async fn early_download_runs_hidden_and_moves_when_confirmed() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("early");
    let m = Manager::open(config).unwrap();
    let mut events = m.subscribe();
    // One connection at 1 MiB/s: 4 s in all, so the answer comes while it runs, and little
    // is on its way (and lost) when the answer stops it to move its files.
    let slow = NewDownload { connections: Some(1), ..early(s.url(&format!("/data?name=early.bin&rate={MIB}"))) };
    let id = m.add(slow).unwrap().id;
    let deadline = Instant::now() + Duration::from_secs(10);
    while m.get(id).unwrap().downloaded < 3 * MIB as u64 / 2 {
        assert!(Instant::now() < deadline, "no progress before the answer");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(m.list().is_empty(), "not in the list before the answer");
    assert!(events.try_recv().is_err(), "no window events before the answer");
    assert_eq!(m.get(id).unwrap().resumable, Some(true));
    assert!(downloads.join("early.bin.part").exists());

    let elsewhere = downloads.join("elsewhere");
    let (before, sent) = (m.get(id).unwrap().downloaded, s.bytes_sent());
    m.confirm_provisional(id, elsewhere.clone(), "renamed.bin".into(), Some("note".into()), true, None).await.unwrap();
    assert!(!downloads.join("early.bin.part").exists() && !downloads.join("early.bin.part.json").exists());
    let started = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("no event after the answer");
    assert_eq!(started.unwrap(), Event::Started(id));
    let list = wait_for(&m, "completion", all_complete).await;
    assert_eq!(list[0].path.as_deref(), Some(elsewhere.join("renamed.bin").as_path()));
    assert_eq!(list[0].description.as_deref(), Some("note"));
    assert_eq!(std::fs::read(elsewhere.join("renamed.bin")).unwrap(), s.data);
    // What had arrived moved along: the download went on from it under the new name.
    let more = s.bytes_sent() - sent;
    assert!(more <= rest(4 * MIB, before), "started over: {more} bytes fetched after {before}");
    assert!(m.confirm_provisional(id, elsewhere, "again.bin".into(), None, true, None).await.is_err(), "answered once");
    m.shutdown().await;
}

/// A small file can finish before the answer: Start then only opens its complete window
/// (moving the file to the chosen name).
#[tokio::test(flavor = "multi_thread")]
async fn early_download_finished_before_the_answer() {
    let s = server(MIB).await;
    let (_dir, config, downloads) = config("early-finished");
    let m = Manager::open(config).unwrap();
    let mut events = m.subscribe();
    let id = m.add(early(s.url("/data?name=small.bin"))).unwrap().id;
    let deadline = Instant::now() + Duration::from_secs(10);
    while m.get(id).unwrap().status != Status::Complete {
        assert!(Instant::now() < deadline, "not finished");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(events.try_recv().is_err(), "no complete window before the answer");
    m.confirm_provisional(id, downloads.clone(), "chosen.bin".into(), None, true, None).await.unwrap();
    let complete = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("no event after the answer");
    assert_eq!(complete.unwrap(), Event::Complete(id));
    let d = m.get(id).unwrap();
    assert_eq!(d.path.as_deref(), Some(downloads.join("chosen.bin").as_path()));
    assert_eq!(std::fs::read(downloads.join("chosen.bin")).unwrap(), s.data);
    assert!(!downloads.join("small.bin").exists());
    m.shutdown().await;
}

/// Download Later keeps what arrived and waits in its queue; Cancel deletes everything;
/// one left unanswered by a crash or shutdown is gone at the next start.
#[tokio::test(flavor = "multi_thread")]
async fn early_download_later_cancel_and_leftovers() {
    let s = server(4 * MIB).await;
    let (_dir, config, downloads) = config("early-later");
    let home = config.home.clone();
    let m = Manager::open(config).unwrap();
    let progress = async |m: &Manager, id: i64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while m.get(id).unwrap().downloaded == 0 {
            assert!(Instant::now() < deadline, "no progress");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let later = m.add(early(s.url(&format!("/data?name=later.bin&rate={}", MIB / 4)))).unwrap().id;
    progress(&m, later).await;
    m.confirm_provisional(later, downloads.clone(), "later.bin".into(), None, false, Some(MAIN_QUEUE)).await.unwrap();
    let d = m.get(later).unwrap();
    assert_eq!(d.status, Status::Waiting);
    assert!(d.downloaded > 0 && downloads.join("later.bin.part.json").exists(), "what arrived is kept");
    assert_eq!(m.list().len(), 1);

    let cancelled = m.add(early(s.url(&format!("/data?name=cancelled.bin&rate={}", MIB / 4)))).unwrap().id;
    progress(&m, cancelled).await;
    m.remove(cancelled, true).await.unwrap();
    assert!(!downloads.join("cancelled.bin.part").exists() && !downloads.join("cancelled.bin.part.json").exists());

    let left = m.add(early(s.url(&format!("/data?name=left.bin&rate={}", MIB / 4)))).unwrap().id;
    progress(&m, left).await;
    m.shutdown().await;
    drop(m);
    let m = reopen(home);
    assert!(m.get(left).is_err(), "unanswered download removed");
    assert!(!downloads.join("left.bin.part").exists() && !downloads.join("left.bin.part.json").exists());
    assert_eq!(m.list().len(), 1, "the queued one stays");
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_to_category_folders_and_remove_cleans_up() {
    let s = server(MIB).await;
    let (_dir, config, downloads) = config("categories");
    let m = Manager::open(config).unwrap();
    let videos = downloads.parent().unwrap().join("my videos");
    assert!(m.set_category_dir("video", "relative".into()).is_err());
    m.set_category_dir("video", videos.clone()).unwrap();
    m.add(new(s.url("/data?name=pack.zip"), true)).unwrap();
    m.add(new(s.url("/data?name=notes.txt"), true)).unwrap();
    m.add(new(s.url("/data?name=clip.mp4"), true)).unwrap();
    let list = wait_for(&m, "completion", all_complete).await;
    assert_eq!(list[2].path.as_deref(), Some(videos.join("clip.mp4").as_path()));
    assert_eq!(list[0].path.as_deref(), Some(downloads.join("Compressed").join("pack.zip").as_path()));
    assert_eq!(list[0].category.as_deref(), Some("Compressed"));
    assert_eq!(list[1].path.as_deref(), Some(downloads.join("notes.txt").as_path()));

    // An unfinished download's parts always go (1 MiB at 4 x 16 KiB/s takes a quarter minute) ...
    let slow = m.add(new(s.url("/data?name=slow.bin&rate=16384"), true)).unwrap().id;
    wait_for(&m, "progress", |l| l[3].downloaded > 0).await;
    m.remove(slow, false).await.unwrap();
    assert_eq!(m.list().len(), 3, "removed before it finished");
    assert!(!downloads.join("slow.bin.part").exists() && !downloads.join("slow.bin.part.json").exists());
    // ... a finished file goes only when asked to.
    m.remove(list[0].id, true).await.unwrap();
    assert!(!downloads.join("Compressed").join("pack.zip").exists());
    m.remove(list[1].id, false).await.unwrap();
    assert_eq!(std::fs::read(downloads.join("notes.txt")).unwrap(), s.data, "removed from the list only");
    assert_eq!(m.list().len(), 1);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_files_are_retried_by_their_queue() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("retries");
    let m = Manager::open(config).unwrap();
    let mut main = m.queues().remove(0);
    main.retries = 2;
    m.update_queue(main).unwrap();
    // A file that does not match the server's hash may come right the next time, and so
    // may a refusal (a quota, a block); a file the server does not have will not, so it
    // gets no retries.
    let wrong = "0".repeat(40);
    let damaged = m.add(new(s.url(&format!("/data?name=d.bin&header=x-bz-content-sha1&value={wrong}")), false)).unwrap().id;
    let refused = m.add(new(s.url("/data?name=r.bin&cookie=wanted"), false)).unwrap().id;
    let missing = m.add(new(s.url("/missing"), false)).unwrap().id;
    m.start_queue(MAIN_QUEUE).unwrap();
    let list = wait_for(&m, "failures", |l| l.iter().all(|d| d.status == Status::Failed)).await;
    let get = |id| list.iter().find(|d| d.id == id).unwrap();
    assert_eq!((get(damaged).attempts, get(damaged).failed_for_good), (2, false));
    assert!(get(damaged).error.as_deref().unwrap().contains("SHA-1"), "{:?}", get(damaged).error);
    assert_eq!((get(refused).attempts, get(refused).failed_for_good), (2, false));
    assert!(get(refused).error.as_deref().unwrap().contains("403"), "{:?}", get(refused).error);
    assert_eq!((get(missing).attempts, get(missing).failed_for_good), (0, true));
    assert!(get(missing).error.as_deref().unwrap().contains("404"), "{:?}", get(missing).error);
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn global_limit_covers_all_downloads() {
    let s = server(MIB).await;
    let (_dir, config, _) = config("limit");
    let m = Manager::open(config).unwrap();
    m.set_speed_limit(MIB as u64).unwrap();
    let started = Instant::now();
    m.add(new(s.url("/data?name=a.bin"), true)).unwrap();
    m.add(new(s.url("/data?name=b.bin"), true)).unwrap();
    wait_for(&m, "completion", all_complete).await;
    let secs = started.elapsed().as_secs_f64();
    // 2 MiB at 1 MiB/s for both together: each with a limit of its own would take 1 s, and
    // the limit counted twice nearly 4 s (the engine's tests have the limiter's exact sums).
    assert!((1.6..3.5).contains(&secs), "2 MiB took {secs:.2} s under a limit of 1 MiB/s for all");
    m.shutdown().await;
}
