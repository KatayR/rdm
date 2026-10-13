//! `rdm-host`: the native messaging host the browser extension starts.
//! Nothing but protocol messages may go to stdout. The browser hides the host's stderr,
//! so starts, requests and replies also go to host.log in the data folder (never cookies).

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use rdm::host::{handle, read_message, write_message};
use serde_json::Value;

fn main() -> ExitCode {
    log(&format!("start {:?}", std::env::args().skip(1).collect::<Vec<_>>()));
    let code = run();
    log(&format!("exit {code:?}"));
    code
}

fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return fail(&e.to_string()),
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let home = manager::home();
    loop {
        let body = match read_message(&mut stdin) {
            Ok(Some(body)) => body,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => return fail(&e.to_string()),
        };
        log(&format!("request {}", summary(&body)));
        let reply = runtime.block_on(handle(&home, &body));
        log(&format!("reply {reply}"));
        if let Err(e) = write_message(&mut stdout, &reply) {
            return fail(&e.to_string());
        }
    }
}

fn fail(error: &str) -> ExitCode {
    eprintln!("rdm-host: {error}");
    log(&format!("error {error}"));
    ExitCode::FAILURE
}

/// Message type and URL only: the rest can hold cookies.
fn summary(body: &[u8]) -> String {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => format!("{} {}", v["type"], v["url"]),
        Err(_) => format!("({} bytes, not JSON)", body.len()),
    }
}

fn log(line: &str) {
    let home = manager::home();
    let _ = std::fs::create_dir_all(&home);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("host.log")) {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "{secs} [{}] {line}", std::process::id());
    }
}
