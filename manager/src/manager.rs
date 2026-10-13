//! The download manager: list, queues, scheduler and speed limits around the engine.
//!
//! All state lives in memory behind one lock and is written through to the store.
//! Lock order: `state`, then `store`. No lock is held across an await. (`ahead`, the
//! pages to list in advance, has its own lock, never held with those.)
//!
//! Here: the shared state, opening and shutting down, events, the list itself and the
//! ticker. The rest by concern: `run` (one download running), `streams` (a video's
//! streams, joined by ffmpeg), `queues`, `settings` (with categories), `file_info`
//! (Download File Info: downloads started before the answer, duplicates, a new address),
//! `files` (files on disk, names, hash check) and `video_pages` (yt-dlp and co.:
//! versions, updates, listing pages).

mod file_info;
mod files;
mod queues;
mod run;
mod settings;
mod streams;
mod video_pages;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveDateTime};
use engine::{CancellationToken, Limiter, Progress};
use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::model::{Category, Download, Event, NewDownload, Queue, Schedule, Settings, Status};
use crate::schedule;
use crate::store::{MAIN_QUEUE, Store};
use crate::tools::Tools;
use crate::video;
use crate::{Error, Result};
use file_info::Move;
use files::delete_files;
use queues::next_position;
use video_pages::{Ahead, keep_yt_dlp_updated};

const TICK: Duration = Duration::from_secs(1);
/// Progress of running downloads goes to the database every this many ticks; the
/// engine's own resume file is what resuming relies on.
const SAVE_EVERY_TICKS: u32 = 5;

pub struct Config {
    /// Folder for the database (and the daemon socket on macOS).
    pub home: PathBuf,
    /// Used when a new database is created; defaults to the system Downloads folder.
    pub downloads_dir: Option<PathBuf>,
    /// Passed to the engine; tests use small files and lower it.
    pub min_split: u64,
    /// Where ffmpeg is: beside the programs (`media::shipped_ffmpeg`).
    pub ffmpeg: PathBuf,
}

impl Config {
    pub fn new(home: PathBuf) -> Self {
        Config { home, downloads_dir: None, min_split: 1 << 20, ffmpeg: crate::media::shipped_ffmpeg() }
    }
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    store: Mutex<Store>,
    global: Arc<Limiter>,
    min_split: u64,
    /// Stops the ticker on shutdown.
    closing: CancellationToken,
    events: broadcast::Sender<Event>,
    /// The runtime `open` ran in. Downloads start on it, so the manager can be called
    /// from any thread, such as a window toolkit's main thread with no runtime of its own.
    runtime: tokio::runtime::Handle,
    /// yt-dlp and Deno for video pages.
    tools: Tools,
    /// Where ffmpeg is (`Config::ffmpeg`).
    ffmpeg: PathBuf,
    /// yt-dlp's listings of video pages asked about lately.
    listings: video::Listings,
    /// Pages to list in advance (the browser saw their video play).
    ahead: Mutex<Ahead>,
}

struct State {
    downloads: BTreeMap<i64, Download>,
    queues: BTreeMap<i64, Queue>,
    categories: Vec<Category>,
    settings: Settings,
    running: HashMap<i64, Running>,
    /// Downloads whose files move for Download File Info's answer, and where to.
    moving: HashMap<i64, Move>,
}

struct Running {
    stop: CancellationToken,
    /// Cancelled by the task once it has recorded its result.
    done: CancellationToken,
    limiter: Arc<Limiter>,
    progress: watch::Receiver<Progress>,
    /// The queue that started it; None when started by hand.
    queue: Option<i64>,
    after_stop: AfterStop,
    last_sample: (Instant, u64),
}

#[derive(Clone, Copy, PartialEq)]
enum AfterStop {
    Pause,
    /// Back to waiting in its queue (the queue was stopped).
    Wait,
    /// Stays "downloading" so the next start of the manager resumes it.
    Shutdown,
    Remove,
}

impl Manager {
    /// Opens the database in `config.home`, resumes downloads that were running when the
    /// manager last stopped, and starts the ticker (progress, schedules). Must be called
    /// inside a Tokio runtime; later calls may come from any thread.
    pub fn open(config: Config) -> Result<Manager> {
        std::fs::create_dir_all(&config.home)?;
        let downloads_dir = config
            .downloads_dir
            .or_else(dirs::download_dir)
            .unwrap_or_else(|| config.home.join("Downloads"));
        let store = Store::open(&config.home.join("rdm.db"), &downloads_dir)?;
        let settings = store.settings()?;
        let state = State {
            downloads: store.downloads()?.into_iter().map(|d| (d.id, d)).collect(),
            queues: store.queues()?.into_iter().map(|q| (q.id, q)).collect(),
            categories: store.categories()?,
            running: HashMap::new(),
            moving: HashMap::new(),
            settings,
        };
        let inner = Arc::new(Inner {
            global: Arc::new(Limiter::new(state.settings.speed_limit)),
            state: Mutex::new(state),
            store: Mutex::new(store),
            min_split: config.min_split,
            closing: CancellationToken::new(),
            events: broadcast::channel(256).0,
            runtime: tokio::runtime::Handle::current(),
            tools: Tools::new(config.home.join("tools"))?,
            ffmpeg: config.ffmpeg,
            listings: video::Listings::default(),
            ahead: Mutex::new(Ahead::default()),
        });
        {
            let mut st = inner.state();
            // Started for a Download File Info window that is gone (the app stopped first).
            let unanswered: Vec<i64> = st.downloads.values().filter(|d| d.provisional).map(|d| d.id).collect();
            for id in unanswered {
                let d = st.downloads.remove(&id).expect("listed above");
                inner.store().delete(id)?;
                if let Err(e) = delete_files(&d, true) {
                    eprintln!("rdm: could not delete the files of unanswered download {id}: {e}");
                }
            }
            let interrupted: Vec<i64> =
                st.downloads.values().filter(|d| d.status == Status::Downloading).map(|d| d.id).collect();
            for id in interrupted {
                inner.start_locked(&mut st, id, None);
            }
            inner.pump_locked(&mut st);
        }
        inner.runtime.spawn(ticker(inner.clone()));
        inner.runtime.spawn(keep_yt_dlp_updated(inner.clone()));
        Ok(Manager { inner })
    }

    pub fn add(&self, new: NewDownload) -> Result<Download> {
        check_url(&new.url)?;
        let mut st = self.inner.state();
        let queue = new.queue.unwrap_or(MAIN_QUEUE);
        if !st.queues.contains_key(&queue) {
            return Err(Error::NotFound(queue));
        }
        let position = next_position(&st.downloads, queue);
        let mut d = Download {
            id: 0,
            url: new.url,
            headers: new.headers,
            file_name: new.file_name,
            dir: new.dir,
            category: None,
            queue,
            position,
            size: None,
            size_estimated: false,
            downloaded: 0,
            status: Status::Waiting,
            connections: new.connections,
            speed_limit: new.speed_limit,
            added: unix_now(),
            last_try: None,
            attempts: 0,
            error: None,
            failed_for_good: false,
            path: None,
            description: new.description,
            video: new.video,
            checked: None,
            file_check: None,
            checking: false,
            provisional: new.provisional,
            placed: false,
            resumable: None,
            new_address: false,
            speed: 0,
            active_connections: 0,
            segments: Vec::new(),
            pieces: None,
        };
        self.inner.store().insert(&mut d, &new.playlists)?;
        let id = d.id;
        st.downloads.insert(id, d);
        if new.start || new.provisional {
            self.inner.start_locked(&mut st, id, None);
        } else {
            self.inner.pump_locked(&mut st);
        }
        Ok(st.downloads[&id].clone())
    }

    /// The list, without downloads still waiting for an answer in Download File Info.
    pub fn list(&self) -> Vec<Download> {
        let st = self.inner.state();
        st.downloads.values().filter(|d| !d.provisional).map(|d| live(&st, d)).collect()
    }

    pub fn get(&self, id: i64) -> Result<Download> {
        let st = self.inner.state();
        st.downloads.get(&id).map(|d| live(&st, d)).ok_or(Error::NotFound(id))
    }

    /// The person's note on a download (Properties); empty clears it.
    pub fn set_description(&self, id: i64, description: Option<String>) -> Result<()> {
        let mut st = self.inner.state();
        let d = st.downloads.get_mut(&id).ok_or(Error::NotFound(id))?;
        d.description = description.filter(|t| !t.trim().is_empty());
        self.inner.persist(d);
        Ok(())
    }

    /// Removes a download from the list, its partial files, and with `delete_file` the
    /// finished file too.
    pub async fn remove(&self, id: i64, delete_file: bool) -> Result<()> {
        loop {
            self.inner.stop_and_wait(id, AfterStop::Remove).await;
            let moving = {
                let mut st = self.inner.state();
                if !st.downloads.contains_key(&id) {
                    return Err(Error::NotFound(id));
                }
                match st.moving.get(&id) {
                    // Its files are moving for Download File Info's answer: removed from where
                    // they end up.
                    Some(m) => Some(m.over.clone()),
                    // Started again meanwhile: stopped first.
                    None if st.running.contains_key(&id) => None,
                    None => {
                        // The store's lock ends with this statement; `persist` below takes it again.
                        let deleted = self.inner.store().delete(id);
                        if let Err(e) = deleted {
                            // Still in the list, and stopped for the removal (or complete,
                            // if it completed all the same: see `finish`).
                            if let Some(d) = st.downloads.get_mut(&id)
                                && d.status == Status::Downloading
                            {
                                d.status = Status::Paused;
                                self.inner.persist(d);
                                if !d.provisional {
                                    let _ = self.inner.events.send(Event::Stopped(id));
                                }
                            }
                            self.inner.pump_locked(&mut st);
                            return Err(e);
                        }
                        let d = st.downloads.remove(&id).ok_or(Error::NotFound(id))?;
                        self.inner.pump_locked(&mut st);
                        drop(st);
                        return delete_files(&d, delete_file);
                    }
                }
            };
            if let Some(over) = moving {
                over.cancelled().await;
            }
        }
    }

    /// Started, finished, failed and stopped downloads, as they happen.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Stops everything. Downloads that were running stay marked as downloading, so the
    /// next `open` resumes them.
    pub async fn shutdown(&self) {
        self.inner.closing.cancel();
        // Files moving for Download File Info's answer are put in place first, so the answer
        // is kept. No new move begins once `closing` is cancelled.
        let moving: Vec<CancellationToken> = self.inner.state().moving.values().map(|m| m.over.clone()).collect();
        if !moving.is_empty() {
            eprintln!("rdm: waiting for {} download(s) to finish moving their files", moving.len());
        }
        for over in moving {
            over.cancelled().await;
        }
        let ids: Vec<i64> = self.inner.state().running.keys().copied().collect();
        for id in ids {
            self.inner.stop_and_wait(id, AfterStop::Shutdown).await;
        }
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    /// ffmpeg, if it is where it ships.
    fn ffmpeg(&self) -> Option<PathBuf> {
        self.ffmpeg.is_file().then(|| self.ffmpeg.clone())
    }

    /// Writes a download to the store. Failing to save must not stop a download, so the
    /// error is reported and the in-memory list stays the truth until the next save.
    fn persist(&self, d: &Download) {
        if let Err(e) = self.store().save(d) {
            eprintln!("rdm: could not save download {}: {e}", d.id);
        }
    }

    /// Live progress, speeds, periodic saves, and queue schedules.
    fn tick(self: &Arc<Self>, count: u32, last_check: NaiveDateTime, now: NaiveDateTime) -> Vec<(i64, bool)> {
        let mut st = self.state();
        let State { downloads, running, queues, .. } = &mut *st;
        for (id, run) in running.iter_mut() {
            let Some(d) = downloads.get_mut(id) else { continue };
            let p = run.progress.borrow().clone();
            let (then, bytes) = run.last_sample;
            let secs = then.elapsed().as_secs_f64();
            if secs > 0.0 {
                let current = p.downloaded.saturating_sub(bytes) as f64 / secs;
                d.speed = ((d.speed as f64 + current) / 2.0) as u64;
            }
            run.last_sample = (Instant::now(), p.downloaded);
            d.downloaded = p.downloaded;
            take_size(d, &p);
            d.active_connections = p.connections;
            d.segments = p.segments;
            d.pieces = p.pieces;
            if count.is_multiple_of(SAVE_EVERY_TICKS) {
                self.persist(d);
            }
        }
        let mut events = Vec::new();
        for q in queues.values() {
            let Schedule { start, stop, days } = &q.schedule;
            if start.as_deref().is_some_and(|t| schedule::crossed(t, *days, last_check, now)) {
                events.push((q.id, true));
            }
            if stop.as_deref().is_some_and(|t| schedule::crossed(t, *days, last_check, now)) {
                events.push((q.id, false));
            }
        }
        events
    }
}

async fn ticker(inner: Arc<Inner>) {
    let mut tick = interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_check = Local::now().naive_local();
    for count in 1u32.. {
        tokio::select! {
            _ = tick.tick() => {}
            _ = inner.closing.cancelled() => return,
        }
        let now = Local::now().naive_local();
        let events = inner.tick(count, last_check, now);
        last_check = now;
        for (queue, start) in events {
            let result = if start { inner.start_queue(queue) } else { inner.stop_queue(queue).await };
            if let Err(e) = result {
                eprintln!("rdm: scheduled {} of queue {queue} failed: {e}", if start { "start" } else { "stop" });
            }
        }
    }
}

/// A copy of `d` with the latest progress of its run, if running: windows see bytes as
/// they arrive, not at the next tick (the speed is still worked out per tick).
fn live(st: &State, d: &Download) -> Download {
    let mut d = d.clone();
    if let Some(run) = st.running.get(&d.id) {
        let p = run.progress.borrow();
        d.downloaded = p.downloaded;
        take_size(&mut d, &p);
        d.active_connections = p.connections;
        d.segments = p.segments.clone();
        d.pieces = p.pieces;
    }
    d
}

/// The size a running download reports, when it knows one.
fn take_size(d: &mut Download, p: &Progress) {
    if p.total.is_some() {
        d.size = p.total;
        d.size_estimated = p.estimated;
    }
}

/// Only web addresses can be downloaded.
fn check_url(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(Error::Invalid(format!("not an http(s) URL: {url}")));
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// For the tests here and in the files below.
#[cfg(test)]
mod testing {
    use super::{Config, Manager};

    /// A manager on a data folder of its own, and a second connection to its database. The
    /// folder goes when the guard (first, so dropped last) is dropped.
    pub(super) fn open(test: &str) -> (testserver::Folder, Manager, rusqlite::Connection) {
        let dir = testserver::Folder::new(test);
        let db = config(&dir).home.join("rdm.db");
        let m = reopen(&dir);
        (dir, m, rusqlite::Connection::open(db).unwrap())
    }

    /// A manager on the data folder in `dir`, as after a restart of one that `open` gave.
    pub(super) fn reopen(dir: &testserver::Folder) -> Manager {
        Manager::open(config(dir)).unwrap()
    }

    fn config(dir: &testserver::Folder) -> Config {
        Config {
            downloads_dir: Some(dir.path().join("downloads")),
            ffmpeg: crate::media::ffmpeg_in_tests(),
            ..Config::new(dir.path().join("home"))
        }
    }

    /// Renames the tables that downloads, settings, categories and queues are saved to (or
    /// back), so saving them fails meanwhile.
    pub(super) fn tables_away(db: &rusqlite::Connection, away: bool) {
        for table in ["downloads", "settings", "categories", "queues"] {
            let (from, to) = match away {
                true => (table.to_owned(), format!("{table}_away")),
                false => (format!("{table}_away"), table.to_owned()),
            };
            db.execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}")).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A removal that cannot be saved is not made: the download stays in the list with its
    /// files, stopped (it was stopped for the removal).
    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_removal_keeps_the_download() {
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, db) = testing::open("unsaved-removal");
        let url = server.url("/data?name=kept.bin&rate=65536");
        let id = m.add(NewDownload { url, start: true, ..NewDownload::default() }).unwrap().id;
        let part = || m.get(id).unwrap().dir.map(|dir| dir.join("kept.bin.part"));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !part().is_some_and(|p| p.is_file()) {
            assert!(std::time::Instant::now() < deadline, "no part file yet");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        testing::tables_away(&db, true);
        assert!(m.remove(id, true).await.is_err());
        assert_eq!(m.get(id).unwrap().status, Status::Paused);
        assert!(part().unwrap().is_file());

        testing::tables_away(&db, false);
        m.remove(id, true).await.unwrap();
        assert!(m.get(id).is_err());
        m.shutdown().await;
    }
}
