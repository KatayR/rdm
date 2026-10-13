use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use engine::CancellationToken;
use manager::Config;
use manager::ipc::{Client, Request, run_daemon};
use serde_json::{Value, json};

/// How long a step may take before the test fails, rather than wait for ever.
const SOON: Duration = Duration::from_secs(60);

/// One request and the daemon's answer to it.
async fn ask(client: &mut Client, request: Request) -> Value {
    tokio::time::timeout(SOON, client.call(&request)).await.expect("no answer from the daemon").unwrap().unwrap()
}

fn send(stdin: &mut impl Write, message: Value) {
    let body = serde_json::to_vec(&message).unwrap();
    stdin.write_all(&(body.len() as u32).to_ne_bytes()).unwrap();
    stdin.write_all(&body).unwrap();
    stdin.flush().unwrap();
}

fn receive(stdout: &mut impl Read) -> Value {
    let mut len = [0u8; 4];
    stdout.read_exact(&mut len).unwrap();
    let mut body = vec![0; u32::from_ne_bytes(len) as usize];
    stdout.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn host_relays_browser_downloads_to_the_daemon() {
    let s = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
    // A folder of its own (also a pipe of its own), removed when `tmp` goes.
    let tmp = testserver::Folder::new("host");
    let root = tmp.path().to_owned();
    let home = root.join("home");
    let config = Config { downloads_dir: Some(root.join("downloads")), min_split: 256 * 1024, ..Config::new(home.clone()) };
    let daemon = tokio::spawn(run_daemon(config, CancellationToken::new()));
    let opening = async {
        while Client::connect(&home).await.is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(SOON, opening).await.expect("the daemon's socket did not open");
    // The person chose to leave .iso files to the browser.
    let mut setup = Client::connect(&home).await.unwrap();
    let mut settings: manager::model::Settings =
        serde_json::from_value(ask(&mut setup, Request::Settings).await).unwrap();
    settings.exclude_types = vec!["iso".into()];
    ask(&mut setup, Request::UpdateSettings(settings)).await;

    // The browser starts the host with the extension's origin as its argument.
    let mut host = Command::new(env!("CARGO_BIN_EXE_rdm-host"))
        .arg("chrome-extension://hmkendmmfmfkmjcocflneimlmlgjbdmm/")
        .env("RDM_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = host.stdin.take().unwrap();
    let mut stdout = host.stdout.take().unwrap();
    let url = s.url("/data?cookie=session%3D42&name=page.zip");
    let page = s.url("/data?size=100&type=text/html%3B%20charset%3Dutf-8");

    let replies = tokio::task::spawn_blocking(move || {
        send(&mut stdin, json!({ "type": "ping" }));
        let ping = receive(&mut stdout);
        // Without the browser's cookie the server refuses, so the browser keeps the download.
        send(&mut stdin, json!({ "type": "add", "url": url, "cookies": null }));
        let refused = receive(&mut stdout);
        send(&mut stdin, json!({
            "type": "add", "url": url, "cookies": "session=42", "referrer": "http://127.0.0.1/page",
            "user_agent": "Mozilla/5.0 test", "file_name": "page.zip"
        }));
        let added = receive(&mut stdout);
        send(&mut stdin, json!({ "type": "add", "url": url, "file_name": "disk.ISO" }));
        let excluded = receive(&mut stdout);
        assert_eq!(excluded, json!({ "ok": false, "excluded": true }));
        // A link that looked like a file but serves a web page stays with the browser.
        send(&mut stdin, json!({ "type": "add", "url": page, "from_link": true }));
        let page = receive(&mut stdout);
        drop(stdin);
        (ping, refused, added, page)
    });
    // A host that does not answer is ended, which ends the read that waits for it.
    let Ok(replies) = tokio::time::timeout(SOON, replies).await else {
        host.kill().unwrap();
        panic!("the host did not answer");
    };
    let (ping, refused, added, page) = replies.unwrap();
    assert_eq!(page["ok"], false, "{page}");
    assert!(page["error"].as_str().unwrap().contains("web page"), "{page}");
    assert_eq!(ping["ok"], true, "{ping}");
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("403"), "{refused}");
    assert_eq!(added["ok"], true, "{added}");
    let ended = async {
        loop {
            match host.try_wait().unwrap() {
                Some(status) => break status,
                None => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    };
    let Ok(status) = tokio::time::timeout(SOON, ended).await else {
        host.kill().unwrap();
        panic!("the host did not end when the browser closed the pipe");
    };
    assert!(status.success(), "{status}");

    let mut client = Client::connect(&home).await.unwrap();
    let finishing = async {
        loop {
            let list = ask(&mut client, Request::List).await;
            let d = &list[0];
            if d["status"] == "complete" {
                assert_eq!(d["headers"][0], json!(["Referer", "http://127.0.0.1/page"]));
                break d["path"].as_str().unwrap().to_owned();
            }
            assert_ne!(d["status"], "failed", "{d}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let path = tokio::time::timeout(SOON, finishing).await.expect("the download did not finish");
    assert!(path.ends_with("Compressed\\page.zip") || path.ends_with("Compressed/page.zip"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), s.data);
    ask(&mut client, Request::Shutdown).await;
    tokio::time::timeout(SOON, daemon).await.expect("the daemon did not stop").unwrap().unwrap();
}
