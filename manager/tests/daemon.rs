use std::time::Duration;

use engine::CancellationToken;
use manager::Config;
use manager::ipc::{Client, Request, UiRequest, is_other_version, listen, run_daemon, serve, socket_name};
use manager::Manager;
use manager::model::{Caught, CaughtKind, NewDownload, Queue, Settings, VideoPage};
use serde_json::Value;
use std::path::PathBuf;

/// How long a step may take before the test fails, rather than wait for ever.
const SOON: Duration = Duration::from_secs(20);

/// One request and the daemon's answer to it.
async fn ask(client: &mut Client, request: Request) -> Result<Value, String> {
    tokio::time::timeout(SOON, client.call(&request)).await.expect("no answer from the daemon").unwrap()
}

/// One field of download `id` as the daemon lists it.
async fn listed(client: &mut Client, id: i64, field: &str) -> Value {
    let list = ask(client, Request::List).await.unwrap();
    let found = list.as_array().unwrap().iter().find(|d| d["id"] == id);
    found.unwrap_or_else(|| panic!("download {id} is not listed: {list}"))[field].clone()
}

/// The daemon answers every request over its socket, and each does what it says: checked
/// by what a later answer shows.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_answers_over_its_socket() {
    let s = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
    // A folder of its own (also a pipe of its own), removed when `tmp` goes.
    let tmp = testserver::Folder::new("daemon");
    let root = tmp.path().to_owned();
    let home = root.join("home");
    let config = Config { downloads_dir: Some(root.join("downloads")), min_split: 256 * 1024, ..Config::new(home.clone()) };
    let daemon = tokio::spawn(run_daemon(config, CancellationToken::new()));

    let connecting = async {
        loop {
            match Client::connect(&home).await {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    };
    let mut client = tokio::time::timeout(SOON, connecting).await.expect("the daemon's socket did not open");
    let second = Config::new(home.clone());
    let refused = tokio::time::timeout(SOON, run_daemon(second, CancellationToken::new())).await;
    let err = refused.expect("a second daemon began to serve the same folder").unwrap_err();
    assert!(err.to_string().contains("already running"), "{err}");

    let add = NewDownload { url: s.url("/data?name=a.bin"), start: true, ..NewDownload::default() };
    let added = ask(&mut client, Request::Add(add)).await.unwrap();
    assert_eq!(added["id"], 1);
    let bad = ask(&mut client, Request::Resume { ids: vec![99] }).await.unwrap_err();
    assert!(bad.contains("99"), "{bad}");
    let finishing = async {
        while listed(&mut client, 1, "status").await != "complete" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(SOON, finishing).await.expect("the download did not finish");

    // A download slow enough to act on (1 MiB at 4 x 4 KiB/s, a minute): paused, resumed,
    // limited.
    let slow = NewDownload { url: s.url("/data?name=slow.bin&rate=4096"), start: true, ..NewDownload::default() };
    let slow = ask(&mut client, Request::Add(slow)).await.unwrap()["id"].as_i64().unwrap();
    ask(&mut client, Request::Pause { ids: vec![slow] }).await.unwrap();
    assert_eq!(listed(&mut client, slow, "status").await, "paused");
    ask(&mut client, Request::Resume { ids: vec![slow] }).await.unwrap();
    assert_eq!(listed(&mut client, slow, "status").await, "downloading");
    ask(&mut client, Request::DownloadLimit { id: slow, rate: 2048 }).await.unwrap();
    assert_eq!(listed(&mut client, slow, "speed_limit").await, 2048);

    // A queue of its own: made, changed, filled, started and stopped.
    let night = ask(&mut client, Request::NewQueue { name: "Night".into() }).await.unwrap()["id"].as_i64().unwrap();
    let queues = ask(&mut client, Request::Queues).await.unwrap();
    let mut queue: Queue = serde_json::from_value(queues[1].clone()).unwrap();
    assert_eq!((queues.as_array().unwrap().len(), queue.id, queue.name.as_str()), (2, night, "Night"));
    queue.parallel = 3;
    ask(&mut client, Request::UpdateQueue(queue)).await.unwrap();
    let waiting = NewDownload { url: s.url("/data?name=queued.bin&rate=4096"), queue: Some(night), ..NewDownload::default() };
    let waiting = ask(&mut client, Request::Add(waiting)).await.unwrap()["id"].as_i64().unwrap();
    ask(&mut client, Request::Move { ids: vec![slow], queue: night }).await.unwrap();
    assert_eq!(listed(&mut client, slow, "queue").await, night);
    ask(&mut client, Request::StartQueue { id: night }).await.unwrap();
    let queues = ask(&mut client, Request::Queues).await.unwrap();
    assert_eq!((&queues[1]["parallel"], &queues[1]["running"]), (&Value::from(3), &Value::from(true)));
    assert_eq!(listed(&mut client, waiting, "status").await, "downloading");
    ask(&mut client, Request::StopQueue { id: night }).await.unwrap();
    assert_eq!(ask(&mut client, Request::Queues).await.unwrap()[1]["running"], false);
    assert_eq!(listed(&mut client, waiting, "status").await, "waiting");
    ask(&mut client, Request::Remove { ids: vec![slow, waiting], delete_file: true }).await.unwrap();
    assert_eq!(ask(&mut client, Request::List).await.unwrap().as_array().unwrap().len(), 1);
    // The finished one leaves the list only; its file stays, as asked.
    let finished = PathBuf::from(listed(&mut client, 1, "path").await.as_str().unwrap());
    ask(&mut client, Request::Remove { ids: vec![1], delete_file: false }).await.unwrap();
    assert_eq!(ask(&mut client, Request::List).await.unwrap().as_array().unwrap().len(), 0);
    assert_eq!(std::fs::read(&finished).unwrap(), s.data, "{}", finished.display());

    // Settings and category folders.
    ask(&mut client, Request::SpeedLimit { rate: 123_456 }).await.unwrap();
    ask(&mut client, Request::Connections { connections: 5 }).await.unwrap();
    let settings = ask(&mut client, Request::Settings).await.unwrap();
    assert_eq!((&settings["speed_limit"], &settings["connections"]), (&Value::from(123_456), &Value::from(5)));
    let mut changed: Settings = serde_json::from_value(settings).unwrap();
    assert!(changed.show_progress, "on in a new data folder");
    changed.show_progress = false;
    ask(&mut client, Request::UpdateSettings(changed)).await.unwrap();
    assert_eq!(ask(&mut client, Request::Settings).await.unwrap()["show_progress"], false);
    let videos = root.join("videos");
    ask(&mut client, Request::CategoryDir { name: "video".into(), dir: videos.clone() }).await.unwrap();
    let categories = ask(&mut client, Request::Categories).await.unwrap();
    let dirs: Vec<&str> = categories.as_array().unwrap().iter().filter_map(|c| c["dir"].as_str()).collect();
    assert!(dirs.len() == 6 && dirs.contains(&videos.to_str().unwrap()), "{categories}");

    // Nothing to list a page with here (no yt-dlp), which is no error; the windows' requests
    // are, for a daemon that has none.
    ask(&mut client, Request::ListAhead { page: "https://example.org/watch".into(), headers: Vec::new() }).await.unwrap();
    let no_window = ask(&mut client, Request::Show).await.unwrap_err();
    assert!(no_window.contains("without windows"), "{no_window}");
    let page = VideoPage { url: "https://example.org/watch".into(), ..VideoPage::default() };
    let no_app = ask(&mut client, Request::ChooseVideo(page)).await.unwrap_err();
    assert!(no_app.contains("needs the RDM app"), "{no_app}");

    // A program of another version (an update that did not replace every program) is told
    // so and gets nothing done; so is one from before connections began with a version.
    let other = Client::connect_as(&home, "0.0.1").await.err().expect("another version was let in");
    assert!(is_other_version(&other), "{other:?}");
    assert!(other.to_string().contains("updated only in part") && other.to_string().contains("version 0.0.1"), "{other}");
    {
        use interprocess::local_socket::tokio::{Stream, prelude::*};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let old = Stream::connect(socket_name(&home).unwrap()).await.unwrap();
        (&old).write_all(b"{\"cmd\":\"speed_limit\",\"rate\":1}\n").await.unwrap();
        let mut lines = BufReader::new(&old).lines();
        let reply = tokio::time::timeout(SOON, lines.next_line()).await.expect("no answer").unwrap().unwrap();
        assert!(reply.contains("updated only in part") && reply.contains("an earlier version"), "{reply}");
        let closed = tokio::time::timeout(SOON, lines.next_line()).await.expect("the connection stayed open").unwrap();
        assert_eq!(closed, None);
    }
    let limit = ask(&mut client, Request::Settings).await.unwrap()["speed_limit"].clone();
    assert_eq!(limit, 123_456, "the refused request was carried out");

    ask(&mut client, Request::Shutdown).await.unwrap();
    tokio::time::timeout(SOON, daemon).await.expect("the daemon did not stop").unwrap().unwrap();
    assert!(Client::connect(&home).await.is_err(), "socket closed after shutdown");
}

/// The next request the socket hands to the interface.
async fn handed(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiRequest>) -> UiRequest {
    tokio::time::timeout(SOON, rx.recv()).await.expect("nothing handed to the interface").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interface_gets_downloads_to_confirm() {
    // A folder of its own (also a pipe of its own), removed when `tmp` goes.
    let tmp = testserver::Folder::new("ui");
    let root = tmp.path().to_owned();
    let home = root.join("home");
    let config = Config { downloads_dir: Some(root.join("downloads")), min_split: 256 * 1024, ..Config::new(home.clone()) };
    let listener = listen(&home).await.unwrap();
    let manager = Manager::open(config).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = CancellationToken::new();
    let served = {
        let (manager, stop) = (manager.clone(), stop.clone());
        tokio::spawn(async move { serve(&manager, listener, stop, Some(tx)).await })
    };
    let mut client = Client::connect(&home).await.unwrap();

    let new = NewDownload { url: "https://example.org/a.zip".into(), ask: true, ..NewDownload::default() };
    let reply = ask(&mut client, Request::Add(new)).await.unwrap();
    assert_eq!(reply["asked"], true);
    match handed(&mut rx).await {
        UiRequest::Ask(new) => assert_eq!(new.url, "https://example.org/a.zip"),
        other => panic!("{other:?}"),
    }
    assert!(manager.list().is_empty(), "nothing is added before the answer");
    ask(&mut client, Request::Show).await.unwrap();
    assert!(matches!(handed(&mut rx).await, UiRequest::ShowMain));

    let played = Caught {
        url: "https://cdn.example/v/master.m3u8".into(),
        kind: CaughtKind::Hls,
        size: None,
        headers: vec![("Referer".into(), "https://player.example/".into())],
        text: Some("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n".into()),
        earlier: true,
    };
    let page = VideoPage {
        url: "https://site.example/watch".into(),
        title: Some("Episode 1".into()),
        headers: vec![("Cookie".into(), "a=b".into())],
        caught: vec![played.clone()],
        player: Some(manager::model::PlayerData { id: "abc".into(), ..Default::default() }),
    };
    assert_eq!(ask(&mut client, Request::ChooseVideo(page)).await.unwrap()["asked"], true);
    match handed(&mut rx).await {
        UiRequest::ChooseVideo(page) => {
            assert_eq!((page.url.as_str(), page.title.as_deref()), ("https://site.example/watch", Some("Episode 1")));
            assert_eq!(page.headers, [("Cookie".to_owned(), "a=b".to_owned())]);
            assert_eq!(page.caught, [played]);
            assert_eq!(page.player.map(|p| p.id).as_deref(), Some("abc"));
        }
        other => panic!("{other:?}"),
    }
    assert!(manager.list().is_empty(), "nothing is added before a quality is chosen");

    stop.cancel();
    tokio::time::timeout(SOON, served).await.expect("the socket did not close").unwrap();
    manager.shutdown().await;
}
