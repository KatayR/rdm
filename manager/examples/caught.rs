//! Replays what the extension caught in a tab:
//! what the Download Video window would offer, and a download of one line of it.
//!
//!   cargo run -p manager --example caught -- list <page.json>
//!   cargo run -p manager --example caught -- get <page.json> <index> <format> <home> <dir>
//!
//! `<page.json>` is the helper's "video" message as the extension builds it (`VideoPage`:
//! `url`, `title`, `headers`, `caught`). `get` runs a manager of its own on the data folder
//! `<home>` (ffmpeg beside this program, or where `RDM_FFMPEG` points), downloads line `<format>` of
//! `caught[<index>]` into `<dir>` and waits for it. Only lines listed from a playlist's text
//! and plain files can be fetched this way; the others need yt-dlp.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use manager::model::{CaughtKind, NewDownload, Status, VideoPage};
use manager::{Config, Manager, caught};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = match args.as_slice() {
        ["list", page] => read(page).map(|page| list(&page)),
        ["get", page, index, format, home, dir] => match (read(page), index.parse()) {
            (Ok(page), Ok(index)) => get(&page, index, format, home.into(), dir.into()).await,
            (Err(e), _) => Err(e),
            (_, Err(_)) => Err(format!("not an index: {index}")),
        },
        _ => Err("usage: caught list <page.json> | caught get <page.json> <index> <format> <home> <dir>".into()),
    };
    match run {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn read(path: &str) -> Result<VideoPage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))
}

/// One line per thing offered, then its qualities.
fn list(page: &VideoPage) {
    let offered = caught::sources(&page.caught);
    println!("{} caught, {} offered", page.caught.len(), offered.len());
    for s in offered {
        let how = match (&s.info, s.kind) {
            (Some(_), _) => "listed from its text",
            (None, CaughtKind::File) => "a file",
            (None, _) => "to ask yt-dlp about",
        };
        let address: String = s.url.chars().take(110).collect();
        println!("[{}] {:?}{} ({how}) {address}", s.index, s.kind, if s.earlier { ", earlier" } else { "" });
        for c in s.info.iter().flat_map(|info| &info.choices) {
            let size = c.size.map(|bytes| format!(", {}{bytes} bytes", if c.approx { "~" } else { "" })).unwrap_or_default();
            println!("      {:<12} {} {} {}{size}", c.format, c.ext, c.label, c.codecs);
        }
    }
}

async fn get(page: &VideoPage, index: usize, format: &str, home: PathBuf, dir: PathBuf) -> Result<(), String> {
    let played = page.caught.get(index).ok_or("no such entry")?;
    let playlists = caught::saved(&page.caught, index);
    let web = played.url.starts_with("http://") || played.url.starts_with("https://");
    let new = NewDownload {
        url: if web { played.url.clone() } else { page.url.clone() },
        headers: played.headers.clone(),
        video: (played.kind != CaughtKind::File).then(|| format.to_owned()),
        dir: Some(dir),
        file_name: Some("caught.mp4".into()),
        start: true,
        playlists,
        ..NewDownload::default()
    };
    let manager = Manager::open(Config::new(home)).map_err(|e| e.to_string())?;
    let id = manager.add(new).map_err(|e| e.to_string())?.id;
    let done = loop {
        let d = manager.get(id).map_err(|e| e.to_string())?;
        if matches!(d.status, Status::Complete | Status::Failed) {
            break d;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    manager.shutdown().await;
    match done.status {
        Status::Complete => {
            println!("complete: {} ({} bytes)", done.path.unwrap_or_default().display(), done.size.unwrap_or(0));
            Ok(())
        }
        _ => Err(format!("failed: {}", done.error.unwrap_or_default())),
    }
}
