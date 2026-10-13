//! Download File Info's side: a download started before the answer, a link already in
//! the list, and an unfinished download continued from a new address.

use std::path::{Path, PathBuf};

use engine::CancellationToken;

use super::files::{move_download, same_or_numbered};
use super::queues::next_position;
use super::{AfterStop, Inner, Manager, State, check_url, live};
use crate::model::{Download, Event, Status};
use crate::store::MAIN_QUEUE;
use crate::{Error, Result};

impl Manager {
    /// A download in the list (not one still waiting for an answer) with exactly this address.
    pub fn find_same_url(&self, url: &str) -> Option<Download> {
        let st = self.inner.state();
        st.downloads.values().find(|d| !d.provisional && d.url == url).map(|d| live(&st, d))
    }

    /// An unfinished download (a plain file, not one waiting for an answer) of the file `name`
    /// with `size` bytes: what a new link for an expired one looks like. `name` may carry the
    /// " (n)" a new download takes beside a download of the same name.
    pub fn find_same_file(&self, name: &str, size: u64) -> Option<Download> {
        let st = self.inner.state();
        st.downloads
            .values()
            .find(|d| {
                !d.provisional
                    && d.video.is_none()
                    && d.status != Status::Complete
                    && d.size == Some(size)
                    && d.file_name.as_deref().is_some_and(|own| same_or_numbered(name, own))
            })
            .map(|d| live(&st, d))
    }

    /// Continues an unfinished download from a new address with the browser's `headers`
    /// (its link expired and the browser has a fresh one for the same file).
    pub async fn refresh_address(&self, id: i64, url: String, headers: Vec<(String, String)>) -> Result<()> {
        check_url(&url)?;
        let running = {
            let st = self.inner.state();
            let d = st.downloads.get(&id).ok_or(Error::NotFound(id))?;
            if d.status == Status::Complete {
                return Err(Error::Invalid(format!("download {id} is already complete")));
            }
            st.running.contains_key(&id)
        };
        if running {
            self.inner.stop_and_wait(id, AfterStop::Pause).await;
        }
        let mut st = self.inner.state();
        let d = st.downloads.get_mut(&id).ok_or(Error::NotFound(id))?;
        d.url = url;
        d.headers = headers;
        d.new_address = true;
        d.error = None;
        d.failed_for_good = false;
        self.inner.persist(d);
        self.inner.start_locked(&mut st, id, None);
        Ok(())
    }

    /// The answer from Download File Info for a download that started while it was open:
    /// `dir` / `name` as chosen there (the files move when they differ from what the
    /// download uses), then start, or wait in `queue` with what it has.
    pub async fn confirm_provisional(
        &self,
        id: i64,
        dir: PathBuf,
        name: String,
        description: Option<String>,
        start: bool,
        queue: Option<i64>,
    ) -> Result<()> {
        self.answer(id, Answer { dir, name, description, start, queue }, move_download).await
    }

    /// `confirm_provisional` with the files moved by `mover`.
    async fn answer(
        &self,
        id: i64,
        answer: Answer,
        mover: impl FnOnce(&mut Download, &Path, &str) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        let Answer { dir, name, description, start, queue } = answer;
        let (moved, running, mut moving) = loop {
            let other_move = {
                let mut st = self.inner.state();
                let d = st.downloads.get(&id).ok_or(Error::NotFound(id))?;
                if !d.provisional || st.moving.contains_key(&id) {
                    return Err(Error::Invalid(format!("download {id} was already answered")));
                }
                if let Some(q) = queue
                    && !st.queues.contains_key(&q)
                {
                    return Err(Error::NotFound(q));
                }
                let moved = d.dir.as_deref() != Some(dir.as_path()) || d.file_name.as_deref() != Some(name.as_str());
                let running = st.running.contains_key(&id);
                if !moved {
                    break (false, running, None);
                }
                // Shutting down waits only for the moves under way when it began.
                if self.inner.closing.is_cancelled() {
                    return Err(Error::Invalid("RDM is stopping; the answer was not kept".into()));
                }
                // Another answer's files are moving to the same place: this one waits its turn,
                // as the two did when a move held the lock.
                match st.moving.values().find(|m| m.dir == dir && m.name.eq_ignore_ascii_case(&name)) {
                    Some(other) => other.over.clone(),
                    None => break (true, running, Some(Moving::start(&self.inner, &mut st, id, &dir, &name))),
                }
            };
            other_move.cancelled().await;
        };
        // The engine holds the part file open: stop it before moving anything.
        if moved && running {
            self.inner.stop_and_wait(id, AfterStop::Pause).await;
        }
        let placed = if moved { Some(self.inner.moved_copy(id, dir, name, mover).await?) } else { None };
        let still_running = {
            let mut st = self.inner.state();
            if let Some(moving) = moving.take() {
                moving.end(&mut st);
            }
            let target = queue.unwrap_or(st.downloads.get(&id).map_or(MAIN_QUEUE, |d| d.queue));
            let position = next_position(&st.downloads, target);
            let running = st.running.contains_key(&id);
            let d = st.downloads.get_mut(&id).ok_or(Error::NotFound(id))?;
            if let Some(placed) = placed {
                d.dir = placed.dir;
                d.file_name = placed.file_name;
                d.path = placed.path;
            }
            d.description = description;
            d.provisional = false;
            if queue.is_some() {
                d.queue = target;
                d.position = position;
            }
            if !start && !running && matches!(d.status, Status::Paused | Status::Failed) {
                d.status = Status::Waiting;
                d.failed_for_good = false;
            }
            self.inner.persist(d);
            let status = d.status;
            // The windows a download gets when it starts, or the one for where it got to.
            match (start, status) {
                (true, Status::Complete) => {
                    let _ = self.inner.events.send(Event::Complete(id));
                }
                (true, _) if running => {
                    let _ = self.inner.events.send(Event::Started(id));
                }
                (true, _) => self.inner.start_locked(&mut st, id, None),
                (false, _) => {}
            }
            running
        };
        if !start && still_running {
            self.inner.stop_and_wait(id, AfterStop::Wait).await;
        } else if !start {
            self.inner.pump_locked(&mut self.inner.state());
        }
        Ok(())
    }
}

/// What Download File Info answered for a download that started while it was open.
struct Answer {
    dir: PathBuf,
    name: String,
    description: Option<String>,
    start: bool,
    queue: Option<i64>,
}

/// Where a download's files are moving for Download File Info's answer (`State::moving`).
pub(super) struct Move {
    pub(super) dir: PathBuf,
    pub(super) name: String,
    /// Cancelled once the move is over, done or not.
    pub(super) over: CancellationToken,
}

/// Download `id`'s files are moving for an answer. Meanwhile a second answer is refused,
/// removing it and shutting down wait for the move, nothing starts it, and the place it
/// moves to counts as taken. Ended under the lock with `end`, or, if the answer fails
/// first, when dropped (not under the lock).
struct Moving<'a> {
    inner: &'a Inner,
    id: i64,
    over: CancellationToken,
    ended: bool,
}

impl<'a> Moving<'a> {
    fn start(inner: &'a Inner, st: &mut State, id: i64, dir: &Path, name: &str) -> Self {
        let over = CancellationToken::new();
        st.moving.insert(id, Move { dir: dir.to_owned(), name: name.to_owned(), over: over.clone() });
        Moving { inner, id, over, ended: false }
    }

    fn end(mut self, st: &mut State) {
        st.moving.remove(&self.id);
        self.over.cancel();
        self.ended = true;
    }
}

impl Drop for Moving<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.inner.state().moving.remove(&self.id);
            self.over.cancel();
        }
    }
}

impl Inner {
    /// A copy of stopped download `id` whose files `mover` has moved to `dir/name`. A move
    /// across drives is a copy, maybe of gigabytes: it runs on the blocking pool with no
    /// lock held, so the manager keeps answering meanwhile.
    async fn moved_copy(
        &self,
        id: i64,
        dir: PathBuf,
        name: String,
        mover: impl FnOnce(&mut Download, &Path, &str) -> Result<()> + Send + 'static,
    ) -> Result<Download> {
        let mut copy = {
            let st = self.state();
            if st.running.contains_key(&id) {
                return Err(Error::Invalid(format!("download {id} is running; its files cannot move")));
            }
            st.downloads.get(&id).cloned().ok_or(Error::NotFound(id))?
        };
        tokio::task::spawn_blocking(move || mover(&mut copy, &dir, &name).map(|()| copy))
            .await
            .map_err(std::io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::manager::testing;
    use crate::model::NewDownload;

    /// An early download of `name` (still waiting for its answer) once it has some bytes.
    async fn early(m: &Manager, server: &testserver::Server, name: &str) -> i64 {
        let url = server.url(&format!("/data?name={name}&rate=262144"));
        let id = m.add(NewDownload { url, provisional: true, ..NewDownload::default() }).unwrap().id;
        let deadline = Instant::now() + Duration::from_secs(10);
        while m.get(id).unwrap().downloaded == 0 {
            assert!(Instant::now() < deadline, "no bytes yet");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        id
    }

    /// Answers download `id` with `to/renamed.bin`, by a move that goes on until the test
    /// sends on the returned `release` (across drives a copy, maybe of gigabytes, takes long
    /// too). Returns once the move has begun.
    async fn answer_slowly(m: &Manager, id: i64, to: &Path) -> (tokio::task::JoinHandle<Result<()>>, mpsc::Sender<()>) {
        let (started, moving) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let slow = move |d: &mut Download, dir: &Path, name: &str| {
            started.send(()).unwrap();
            // A timeout, so a failing test cannot leave the move hanging.
            let _ = released.recv_timeout(Duration::from_secs(10));
            move_download(d, dir, name)
        };
        let answer = Answer { dir: to.to_owned(), name: "renamed.bin".into(), description: None, start: false, queue: None };
        let answering = tokio::spawn({
            let m = m.clone();
            async move { m.answer(id, answer, slow).await }
        });
        tokio::task::spawn_blocking(move || moving.recv_timeout(Duration::from_secs(10)).unwrap()).await.unwrap();
        (answering, release)
    }

    /// A running early download whose files are moving (`answer_slowly`) from `from` to `to`.
    struct SlowMove {
        m: Manager,
        id: i64,
        from: PathBuf,
        to: PathBuf,
        answering: tokio::task::JoinHandle<Result<()>>,
        release: mpsc::Sender<()>,
        _server: testserver::Server,
        /// Last (fields are dropped in order), so the folder goes after the manager. The
        /// tests take it apart with `_dir` first, which keeps that order (bindings are
        /// dropped in reverse).
        _dir: testserver::Folder,
    }

    async fn slow_move(test: &str) -> SlowMove {
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (dir, m, _db) = testing::open(test);
        let id = early(&m, &server, "early.bin").await;
        let from = m.get(id).unwrap().dir.unwrap();
        let to = from.join("chosen");
        let (answering, release) = answer_slowly(&m, id, &to).await;
        SlowMove { _dir: dir, m, id, from, to, answering, release, _server: server }
    }

    /// While the files move the manager answers at once, a second answer is refused, nothing
    /// starts the download, and removing it waits for the move, then removes the files from
    /// where they ended up.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_manager_answers_while_files_move() {
        let SlowMove { _dir, m, id, from, to, answering, release, _server } = slow_move("slow-move").await;
        let asked = Instant::now();
        assert_eq!(m.list().len(), 0, "still waiting for its answer");
        assert!(m.get(id).is_ok());
        let again = m.confirm_provisional(id, from.clone(), "other.bin".into(), None, true, None).await;
        assert!(again.is_err_and(|e| e.to_string().contains("already answered")));
        assert!(asked.elapsed() < Duration::from_millis(500), "waited {:?} for the manager", asked.elapsed());
        m.resume(id).unwrap();
        assert_ne!(m.get(id).unwrap().status, Status::Downloading, "started while its files move");
        let removing = tokio::spawn({
            let m = m.clone();
            async move { m.remove(id, true).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!removing.is_finished(), "removing waits for the move");

        release.send(()).unwrap();
        answering.await.unwrap().unwrap();
        removing.await.unwrap().unwrap();
        assert!(m.get(id).is_err());
        for left in [from.join("early.bin.part"), to.join("renamed.bin.part"), to.join("renamed.bin.part.json")] {
            assert!(!left.exists(), "{} left behind", left.display());
        }
        m.shutdown().await;
    }

    /// Shutting down waits for the files to be in place, so the answer is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutting_down_waits_for_files_to_move() {
        let SlowMove { _dir, m, id, to, answering, release, .. } = slow_move("slow-move-shutdown").await;
        let stopping = tokio::spawn({
            let m = m.clone();
            async move { m.shutdown().await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!stopping.is_finished(), "shutting down waits for the move");
        release.send(()).unwrap();
        stopping.await.unwrap();
        assert!(to.join("renamed.bin.part").is_file(), "moved before the manager stopped");
        answering.await.unwrap().unwrap();
        let d = m.get(id).unwrap();
        assert!(!d.provisional && d.dir.as_deref() == Some(to.as_path()));
        // Kept on disk too: unanswered, it would be deleted at the next start.
        drop(m);
        let m = testing::reopen(&_dir);
        let d = m.get(id).expect("the answered download is still there after a restart");
        assert!(!d.provisional && d.dir.as_deref() == Some(to.as_path()));
        assert!(to.join("renamed.bin.part").is_file());
        m.shutdown().await;
    }

    /// While the files move, the place they move to counts as taken: a new download of that
    /// name in that folder takes "(1)", and another answer for the same place waits its turn,
    /// then finds it in use (as one after the other).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_place_files_move_to_is_taken() {
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 4 << 20).await.unwrap();
        let (_dir, m, _db) = testing::open("slow-move-place");
        let first = early(&m, &server, "first.bin").await;
        let second = early(&m, &server, "second.bin").await;
        let to = m.get(first).unwrap().dir.unwrap().join("chosen");
        let (answering, release) = answer_slowly(&m, first, &to).await;

        let answering_too = tokio::spawn({
            let (m, to) = (m.clone(), to.clone());
            async move { m.confirm_provisional(second, to, "renamed.bin".into(), None, false, None).await }
        });
        let url = server.url("/data?name=renamed.bin&rate=262144");
        let new = m.add(NewDownload { url, dir: Some(to.clone()), start: true, ..NewDownload::default() }).unwrap().id;
        let deadline = Instant::now() + Duration::from_secs(10);
        while m.get(new).unwrap().file_name.is_none() {
            assert!(Instant::now() < deadline, "not placed while the files move");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(m.get(new).unwrap().file_name.as_deref(), Some("renamed (1).bin"));
        assert!(!answering_too.is_finished(), "the second answer waits for the first move");

        release.send(()).unwrap();
        answering.await.unwrap().unwrap();
        let second_answer = answering_too.await.unwrap();
        assert!(second_answer.is_err_and(|e| e.to_string().contains("in use by another download")));
        assert!(m.get(second).unwrap().provisional, "still waiting for an answer it can keep");
        m.shutdown().await;
    }

    /// A move that fails, or whose mover panics, leaves the download unanswered where it was,
    /// and nothing waits on it: it can be answered again, and shutting down does not hang.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_move_can_be_answered_again() {
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, _db) = testing::open("failed-move");
        let id = early(&m, &server, "early.bin").await;
        let to = m.get(id).unwrap().dir.unwrap().join("chosen");
        let answer = || Answer { dir: to.clone(), name: "renamed.bin".into(), description: None, start: false, queue: None };

        let failing = |_: &mut Download, _: &Path, _: &str| Err(Error::Invalid("disk full".into()));
        assert!(m.answer(id, answer(), failing).await.is_err_and(|e| e.to_string().contains("disk full")));
        let panicking = |_: &mut Download, _: &Path, _: &str| -> Result<()> { panic!("mover panicked") };
        assert!(m.answer(id, answer(), panicking).await.is_err());
        assert!(m.get(id).unwrap().provisional && m.inner.state().moving.is_empty());

        m.confirm_provisional(id, to.clone(), "renamed.bin".into(), None, false, None).await.unwrap();
        assert!(to.join("renamed.bin.part").is_file());
        tokio::time::timeout(Duration::from_secs(10), m.shutdown()).await.expect("shutdown hung");
    }

    /// Once shutting down has begun, an answer that would move files is refused: shutting
    /// down waits only for the moves already under way.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_move_begins_once_shutting_down() {
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, _db) = testing::open("move-after-shutdown");
        let id = early(&m, &server, "early.bin").await;
        let to = m.get(id).unwrap().dir.unwrap().join("chosen");
        m.shutdown().await;
        let refused = m.confirm_provisional(id, to.clone(), "renamed.bin".into(), None, true, None).await;
        assert!(refused.is_err_and(|e| e.to_string().contains("stopping")));
        assert!(!to.join("renamed.bin.part").exists());
    }
}
