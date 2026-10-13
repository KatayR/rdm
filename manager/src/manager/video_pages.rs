//! Video pages: the outside programs for video downloads (yt-dlp, Deno: versions and
//! updates), a page's quality list, and listing a page in advance.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::{MissedTickBehavior, interval};

use super::{Inner, Manager, unix_now};
use crate::model::{ToolInfo, ToolUpdate};
use crate::tools::{self, Tool, Update};
use crate::video;
use crate::Result;

impl Manager {
    /// The outside programs for video downloads, as installed.
    pub fn tools(&self) -> Vec<ToolInfo> {
        Tool::ALL
            .into_iter()
            .map(|tool| {
                let installed = self.inner.tools.installed(tool);
                ToolInfo {
                    name: tool.name().into(),
                    nightly: installed.as_ref().is_some_and(|i| i.nightly),
                    version: installed.map(|i| i.version),
                }
            })
            .collect()
    }

    /// Installs the newest release of each program (Options' Update now), keeping
    /// yt-dlp's channel.
    pub async fn update_tools(&self) -> Vec<ToolUpdate> {
        let mut results = Vec::new();
        for tool in Tool::ALL {
            let nightly = self.inner.tools.installed(tool).is_some_and(|i| i.nightly);
            let (ok, text) = match self.inner.tools.update(tool, nightly).await {
                Ok(Update::Installed(v)) => (true, format!("{v} installed")),
                Ok(Update::UpToDate(v)) => (true, format!("{v} is the newest")),
                Err(e) => (false, e.to_string()),
            };
            results.push(ToolUpdate { name: tool.name().into(), ok, text });
        }
        results
    }

    /// The qualities a video page offers, for the person to choose one.
    pub async fn video_choices(&self, page: &str, headers: &[(String, String)]) -> Result<video::VideoInfo> {
        video::choices(&self.inner.tools, &self.inner.listings, page, headers).await
    }

    /// Lists a video page in the background (the browser saw its video play), so its
    /// quality list is ready, or under way, when asked for. One page at a time; of the
    /// pages reported meanwhile only the latest is listed next. Failures are only logged.
    pub fn list_ahead(&self, page: String, headers: Vec<(String, String)>) {
        if self.inner.tools.path(Tool::YtDlp).is_none() || !self.inner.ahead.lock().unwrap().push(page, headers) {
            return;
        }
        let inner = self.inner.clone();
        self.inner.runtime.spawn(async move {
            loop {
                let next = inner.ahead.lock().unwrap().next();
                let Some((page, headers)) = next else { break };
                if let Err(e) = video::list_ahead(&inner.tools, &inner.listings, &page, &headers).await {
                    eprintln!("rdm: listing {page} in advance: {e}");
                }
            }
        });
    }
}

/// Keeps yt-dlp up to date: once an hour, a check when the last one is a day old (and the
/// setting is on). A failed check (offline, yt-dlp busy) is tried again the next hour.
pub(super) async fn keep_yt_dlp_updated(inner: Arc<Inner>) {
    let mut every = interval(Duration::from_secs(60 * 60));
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            _ = inner.closing.cancelled() => return,
        }
        let Some(installed) = inner.tools.installed(Tool::YtDlp) else { continue };
        if !inner.state().settings.auto_update_tools || !tools::due(installed.checked, unix_now()) {
            continue;
        }
        match inner.tools.update(Tool::YtDlp, installed.nightly).await {
            Ok(Update::Installed(v)) => eprintln!("rdm: yt-dlp updated to {v}"),
            Ok(Update::UpToDate(_)) => {}
            Err(e) => eprintln!("rdm: yt-dlp update check failed: {e}"),
        }
    }
}

/// The pages waiting to be listed in advance: only the latest, since one reported while
/// another runs replaces the one before it (the person moved on).
#[derive(Default)]
pub(super) struct Ahead {
    next: Option<(String, Vec<(String, String)>)>,
    running: bool,
}

impl Ahead {
    /// Notes `page`; true when no listing runs, so one has to start.
    fn push(&mut self, page: String, headers: Vec<(String, String)>) -> bool {
        self.next = Some((page, headers));
        !std::mem::replace(&mut self.running, true)
    }

    /// The page to list next; None ends the running one.
    fn next(&mut self) -> Option<(String, Vec<(String, String)>)> {
        let next = self.next.take();
        self.running = next.is_some();
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_latest_page_waits_to_be_listed() {
        let mut ahead = Ahead::default();
        let page = |p: &str| (p.to_owned(), Vec::new());
        assert!(ahead.push("a".into(), Vec::new()), "nothing runs: start");
        assert_eq!(ahead.next(), Some(page("a")));
        assert!(!ahead.push("b".into(), Vec::new()), "a runs: wait");
        assert!(!ahead.push("c".into(), Vec::new()));
        assert_eq!(ahead.next(), Some(page("c")), "b was passed over");
        assert_eq!(ahead.next(), None);
        assert!(ahead.push("d".into(), Vec::new()), "the runner ended: start again");
    }
}
