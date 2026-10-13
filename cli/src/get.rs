//! `rdm get`: one download in the foreground, without the daemon.

use std::collections::VecDeque;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use engine::{CancellationToken, Error, Options, Progress, download};
use tokio::sync::watch;

use crate::fmt::{clock, human};

pub async fn run(url: &str, opts: Options) -> ExitCode {
    let (tx, rx) = watch::channel(Progress::default());
    let started = Instant::now();
    let meter = tokio::spawn(show_progress(rx));
    let stop = CancellationToken::new();
    let on_ctrl_c = stop.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    let result = download(url, &opts, &tx, &stop).await;
    meter.abort();
    eprintln!();
    match result {
        Ok(out) => {
            let secs = started.elapsed().as_secs_f64();
            let fetched = (out.size - out.resumed) as f64;
            let mut line = format!(
                "saved {} ({}) in {secs:.1} s, {}/s, {} segment(s)",
                out.path.display(),
                human(out.size as f64),
                human(fetched / secs),
                out.segments
            );
            if out.resumed > 0 {
                line += &format!(", resumed from {}", human(out.resumed as f64));
            }
            if out.refused > 0 {
                line += &format!(", server turned away {} connection(s)", out.refused);
            }
            if !out.ranges {
                line += ", server has no range support (one connection)";
            }
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(Error::Cancelled) => {
            eprintln!("stopped; run the same command again to resume");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One status line, redrawn as progress arrives; speed is averaged over the last 3 s.
async fn show_progress(mut rx: watch::Receiver<Progress>) {
    let mut window: VecDeque<(Instant, u64)> = VecDeque::new();
    while rx.changed().await.is_ok() {
        let p = rx.borrow_and_update().clone();
        let now = Instant::now();
        window.push_back((now, p.downloaded));
        while window.front().is_some_and(|&(t, _)| now - t > Duration::from_secs(3)) {
            window.pop_front();
        }
        let (t0, b0) = window[0];
        let dt = (now - t0).as_secs_f64();
        let speed = if dt > 0.0 { (p.downloaded - b0) as f64 / dt } else { 0.0 };
        let line = match p.total {
            Some(total) if total > 0 => {
                let eta = if speed > 0.0 { clock((total - p.downloaded) as f64 / speed) } else { "--:--".into() };
                format!(
                    "{:5.1}%  {} / {}  {}/s  {} conn  ETA {eta}",
                    p.downloaded as f64 * 100.0 / total as f64,
                    human(p.downloaded as f64),
                    human(total as f64),
                    human(speed),
                    p.connections
                )
            }
            _ => format!("{}  {}/s", human(p.downloaded as f64), human(speed)),
        };
        eprint!("\r{line:<72}");
        let _ = std::io::stderr().flush();
    }
}
