//! One download running: started, stopped, placed once the server answers, and its
//! result recorded (then the check by file type); plain files here, a video's streams in
//! `streams`.

use std::path::PathBuf;
use std::sync::Arc;

use engine::{CancellationToken, Limiter, Options, Progress};
use tokio::sync::watch;
use tokio::time::Instant;

use super::files::free_name;
use super::streams::transfer_video;
use super::{AfterStop, Inner, Manager, Running, State, take_size, unix_now};
use crate::categories;
use crate::filecheck;
use crate::model::{Event, FileCheck, Status};
use crate::{Error, Result};

impl Manager {
    /// Starts a download by hand, outside its queue's limit (IDM's "Resume").
    pub fn resume(&self, id: i64) -> Result<()> {
        let mut st = self.inner.state();
        let d = st.downloads.get(&id).ok_or(Error::NotFound(id))?;
        if d.status == Status::Complete {
            return Err(Error::Invalid(format!("download {id} is already complete")));
        }
        if !st.running.contains_key(&id) {
            self.inner.start_locked(&mut st, id, None);
        }
        Ok(())
    }

    /// Stops a download; queues skip it until it is resumed. Returns once it has stopped.
    pub async fn pause(&self, id: i64) -> Result<()> {
        let running = {
            let mut st = self.inner.state();
            let running = st.running.contains_key(&id);
            let d = st.downloads.get_mut(&id).ok_or(Error::NotFound(id))?;
            if !running && d.status == Status::Waiting {
                d.status = Status::Paused;
                self.inner.persist(d);
            }
            running
        };
        if running {
            self.inner.stop_and_wait(id, AfterStop::Pause).await;
        }
        Ok(())
    }
}

impl Inner {
    /// Starts download `id` unless it runs, is complete, its files are moving, or the manager
    /// is shutting down (it would not be stopped).
    pub(super) fn start_locked(self: &Arc<Self>, st: &mut State, id: i64, queue: Option<i64>) {
        let connections = st.settings.connections;
        let Some(d) = st.downloads.get_mut(&id) else { return };
        if st.running.contains_key(&id)
            || st.moving.contains_key(&id)
            || d.status == Status::Complete
            || self.closing.is_cancelled()
        {
            return;
        }
        d.status = Status::Downloading;
        d.last_try = Some(unix_now());
        d.error = None;
        d.failed_for_good = false;
        self.persist(d);
        let (tx, rx) =
            watch::channel(Progress { downloaded: d.downloaded, total: d.size, estimated: d.size_estimated, ..Progress::default() });
        let run = Running {
            stop: CancellationToken::new(),
            done: CancellationToken::new(),
            limiter: Arc::new(Limiter::new(d.speed_limit.unwrap_or(0))),
            progress: rx,
            queue,
            after_stop: AfterStop::Pause,
            last_sample: (Instant::now(), d.downloaded),
        };
        let job = Job {
            url: d.url.clone(),
            video: d.video.clone(),
            headers: d.headers.clone(),
            new_address: std::mem::take(&mut d.new_address),
            connections: d.connections.unwrap_or(connections).into(),
            stop: run.stop.clone(),
            done: run.done.clone(),
            limiter: run.limiter.clone(),
        };
        let provisional = d.provisional;
        st.running.insert(id, run);
        if !provisional {
            let _ = self.events.send(Event::Started(id));
        }
        self.runtime.spawn(run_download(self.clone(), id, job, tx));
    }

    /// Asks a running download to stop and waits until it has recorded its result.
    pub(super) async fn stop_and_wait(&self, id: i64, after: AfterStop) {
        let done = {
            let mut st = self.state();
            let Some(run) = st.running.get_mut(&id) else { return };
            run.after_stop = after;
            run.stop.cancel();
            run.done.clone()
        };
        done.cancelled().await;
    }

    /// Picks the file name, category and folder once the server (or yt-dlp) has been asked;
    /// `found` is the name it suggests; `estimated` marks a guessed size.
    pub(super) fn place(&self, id: i64, found: &str, size: Option<u64>, estimated: bool) -> Option<(String, PathBuf)> {
        let mut st = self.state();
        let State { downloads, categories, settings, moving, .. } = &mut *st;
        let d = downloads.get(&id)?;
        let name = d.file_name.clone().unwrap_or_else(|| found.to_owned());
        let category = categories::for_file(categories, &name);
        let dir = d.dir.clone().or_else(|| category.map(|c| c.dir.clone())).unwrap_or_else(|| settings.downloads_dir.clone());
        // Another unfinished download of that name in that folder writes the same part file
        // (a duplicate, or an early start for a file already being downloaded): this one
        // takes a free name. A video's streams have names of their own.
        let name = if d.video.is_none() { free_name(downloads, moving, id, &dir, &name) } else { name };
        let d = downloads.get_mut(&id)?;
        if d.category.is_none() {
            d.category = category.map(|c| c.name.clone());
        }
        d.dir = Some(dir.clone());
        d.file_name = Some(name.clone());
        d.placed = true;
        d.size = size;
        d.size_estimated = estimated;
        self.persist(d);
        Some((name, dir))
    }

    /// Records how a download task ended, tells of it, and lets the queues start what comes
    /// next. A download stopped for its removal is recorded only if it completed all the
    /// same (every byte was in when the stop came), so that `remove` knows the finished
    /// file; nobody is told, and the file is not checked: it may be deleted in a moment.
    fn finish(self: &Arc<Self>, id: i64, result: Result<Finished>) {
        let mut st = self.state();
        let Some(run) = st.running.remove(&id) else { return };
        let removed = run.after_stop == AfterStop::Remove;
        if removed && result.is_err() {
            return;
        }
        let retries = run.queue.and_then(|q| st.queues.get(&q)).map(|q| q.retries);
        let Some(d) = st.downloads.get_mut(&id) else { return };
        let last = run.progress.borrow().clone();
        let check = match &result {
            Ok(out) if !removed => filecheck::kind(&out.path, out.video_length).map(|kind| (kind, out.path.clone())),
            _ => None,
        };
        d.downloaded = last.downloaded;
        // The last report may have come after the last tick (all pieces in, then ffmpeg failed).
        take_size(d, &last);
        d.speed = 0;
        d.active_connections = 0;
        d.segments.clear();
        d.pieces = None;
        match result {
            Ok(out) => {
                d.status = Status::Complete;
                d.size = Some(out.size);
                d.size_estimated = false;
                d.downloaded = out.size;
                d.checked = out.checked;
                d.checking = check.is_some();
                // A video may end up in another container than first named (Matroska).
                d.file_name = out.path.file_name().map(|n| n.to_string_lossy().into_owned()).or(d.file_name.take());
                d.path = Some(out.path);
            }
            Err(Error::Download(engine::Error::Cancelled)) => {
                d.status = match run.after_stop {
                    AfterStop::Pause => Status::Paused,
                    AfterStop::Wait => Status::Waiting,
                    AfterStop::Shutdown => Status::Downloading,
                    AfterStop::Remove => unreachable!("returned above"),
                };
            }
            Err(e) => {
                d.error = Some(e.to_string());
                // Only a plain download's first answer can say this (a video's stream
                // addresses are fetched again at every start, so they may work later).
                d.failed_for_good = matches!(e, Error::Gone(_));
                d.status = match retries {
                    Some(retries) if d.attempts < retries && !d.failed_for_good => {
                        d.attempts += 1;
                        Status::Waiting
                    }
                    _ => Status::Failed,
                };
            }
        }
        self.persist(d);
        if removed {
            return;
        }
        let event = match d.status {
            // Its windows open once Download File Info is answered.
            _ if d.provisional => None,
            Status::Complete => Some(Event::Complete(id)),
            Status::Failed => Some(Event::Failed(id)),
            Status::Paused | Status::Waiting => Some(Event::Stopped(id)),
            Status::Downloading => None,
        };
        if let Some(event) = event {
            let _ = self.events.send(event);
        }
        if run.after_stop != AfterStop::Shutdown {
            self.pump_locked(&mut st);
        }
        // After the download counts as complete: nothing waits for it, and it only warns.
        if let Some((kind, path)) = check {
            let (inner, ffmpeg) = (self.clone(), self.ffmpeg());
            tokio::spawn(async move {
                let found = filecheck::run(kind, path, ffmpeg).await;
                inner.file_checked(id, found);
            });
        }
    }

    fn file_checked(&self, id: i64, found: Option<FileCheck>) {
        let mut st = self.state();
        let Some(d) = st.downloads.get_mut(&id) else { return };
        d.checking = false;
        d.file_check = found;
        self.persist(d);
    }
}

pub(super) struct Job {
    pub(super) url: String,
    video: Option<String>,
    pub(super) headers: Vec<(String, String)>,
    /// The address was just replaced (`Options::new_address`).
    new_address: bool,
    connections: usize,
    pub(super) stop: CancellationToken,
    done: CancellationToken,
    limiter: Arc<Limiter>,
}

impl Job {
    pub(super) fn options(&self, inner: &Inner, dir: PathBuf, file_name: String, headers: Vec<(String, String)>) -> Options {
        Options {
            connections: self.connections,
            dir,
            file_name: Some(file_name),
            min_split: inner.min_split,
            headers,
            limiters: vec![inner.global.clone(), self.limiter.clone()],
            max_request: None,
            new_address: self.new_address,
        }
    }

    /// Runs `work` unless the download is stopped first.
    pub(super) async fn unless_stopped<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            r = work => r,
            _ = self.stop.cancelled() => Err(engine::Error::Cancelled.into()),
        }
    }
}

/// A finished download's file.
pub(super) struct Finished {
    pub(super) path: PathBuf,
    pub(super) size: u64,
    pub(super) checked: Option<String>,
    /// A video's length as the site or the playlist gave it, for the file check.
    pub(super) video_length: Option<f64>,
}

async fn run_download(inner: Arc<Inner>, id: i64, job: Job, progress: watch::Sender<Progress>) {
    let mut restarted = false;
    let result = loop {
        let result = match job.video.clone() {
            Some(format) => transfer_video(&inner, id, &job, &format, &progress).await,
            None => transfer(&inner, id, &job, &progress).await,
        };
        // A file replaced on the server starts over by itself (the engine dropped what it
        // had); once, so a file that keeps changing fails instead.
        match result {
            Err(Error::Download(engine::Error::Changed(_))) if !restarted => restarted = true,
            result => break result,
        }
    };
    inner.finish(id, result);
    job.done.cancel();
}

async fn transfer(inner: &Arc<Inner>, id: i64, job: &Job, progress: &watch::Sender<Progress>) -> Result<Finished> {
    let probe = job
        .unless_stopped(async {
            engine::probe(&job.url, &job.headers).await.map_err(|e| if e.is_gone() { Error::Gone(e) } else { e.into() })
        })
        .await?;
    if let Some(d) = inner.state().downloads.get_mut(&id) {
        d.resumable = Some(probe.ranges);
    }
    let Some((name, dir)) = inner.place(id, &probe.file_name, probe.size, false) else { return Err(engine::Error::Cancelled.into()) };
    let opts = job.options(inner, dir, name, Vec::new());
    let out = engine::download_probed(probe, &opts, progress, &job.stop).await?;
    Ok(Finished { path: out.path, size: out.size, checked: out.checked, video_length: None })
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use super::*;
    use crate::model::NewDownload;

    /// Download `id` as its task leaves it when every byte was in before the stop for its
    /// removal came: placed, downloading, stopped for a removal, and finished. Gives its
    /// file (a zip archive by its name, so a complete download's file would be tested).
    fn completes_while_removed(m: &Manager, id: i64) -> PathBuf {
        let (name, dir) = m.inner.place(id, "pack.zip", Some(4), false).unwrap();
        let file = dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "four").unwrap();
        m.inner.state().downloads.get_mut(&id).unwrap().status = Status::Downloading;
        let run = Running {
            stop: CancellationToken::new(),
            done: CancellationToken::new(),
            limiter: Arc::new(Limiter::new(0)),
            progress: watch::channel(Progress::default()).1,
            queue: None,
            after_stop: AfterStop::Remove,
            last_sample: (Instant::now(), 0),
        };
        m.inner.state().running.insert(id, run);
        m.inner.finish(id, Ok(Finished { path: file.clone(), size: 4, checked: None, video_length: None }));
        file
    }

    /// A download that completes although it was stopped for its removal goes as a complete
    /// one does: its file is deleted when asked to and stays when not, and nobody hears of a
    /// completion. A removal that cannot be saved leaves it in the list, saved as complete.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_completing_as_it_is_removed_goes_with_its_file() {
        let (dir, m, db) = testing::open("removed-as-it-completes");
        let add = || m.add(NewDownload { url: "http://127.0.0.1:9/pack.zip".into(), ..NewDownload::default() }).unwrap().id;

        let id = add();
        let mut events = m.subscribe();
        let file = completes_while_removed(&m, id);
        assert!(file.starts_with(dir.path()));
        assert!(events.try_recv().is_err(), "a completion was announced");
        let d = m.get(id).unwrap();
        assert_eq!((d.status, d.path.as_deref(), d.size, d.checking), (Status::Complete, Some(file.as_path()), Some(4), false));
        let saved: String = db.query_row("SELECT status FROM downloads WHERE id = ?1", [id], |row| row.get(0)).unwrap();
        assert_eq!(saved, Status::Complete.as_str());
        testing::tables_away(&db, true);
        assert!(m.remove(id, true).await.is_err());
        assert!(m.get(id).unwrap().status == Status::Complete && file.is_file());
        testing::tables_away(&db, false);
        m.remove(id, true).await.unwrap();
        assert!(m.get(id).is_err() && !file.exists(), "removed with its file");

        let id = add();
        assert_eq!(completes_while_removed(&m, id), file);
        m.remove(id, false).await.unwrap();
        assert!(m.get(id).is_err() && file.is_file(), "removed from the list only");
        m.shutdown().await;
    }
}
