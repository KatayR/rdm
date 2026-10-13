//! Queues: what is in them, starting and stopping them, and starting their waiting
//! downloads up to each queue's limit.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{AfterStop, Inner, Manager, State};
use crate::model::{Download, Queue, Schedule, Status};
use crate::schedule;
use crate::{Error, Result};

impl Manager {
    /// Moves downloads to the end of another queue, with their retries afresh.
    pub fn move_to_queue(&self, ids: &[i64], queue: i64) -> Result<()> {
        let mut st = self.inner.state();
        if !st.queues.contains_key(&queue) {
            return Err(Error::NotFound(queue));
        }
        for (position, id) in (next_position(&st.downloads, queue)..).zip(ids) {
            let d = st.downloads.get_mut(id).ok_or(Error::NotFound(*id))?;
            d.queue = queue;
            d.position = position;
            d.attempts = 0;
            self.inner.persist(d);
        }
        self.inner.pump_locked(&mut st);
        Ok(())
    }

    pub fn queues(&self) -> Vec<Queue> {
        self.inner.state().queues.values().cloned().collect()
    }

    pub fn new_queue(&self, name: &str) -> Result<Queue> {
        let mut q = Queue {
            id: 0,
            name: name.to_owned(),
            parallel: 1,
            retries: 10,
            schedule: Schedule::default(),
            running: false,
        };
        let mut st = self.inner.state();
        self.inner.store().insert_queue(&mut q)?;
        st.queues.insert(q.id, q.clone());
        Ok(q)
    }

    /// Changes a queue's name, parallel files, retries and schedule (not whether it runs).
    pub fn update_queue(&self, update: Queue) -> Result<Queue> {
        for time in [&update.schedule.start, &update.schedule.stop].into_iter().flatten() {
            if schedule::parse_time(time).is_none() {
                return Err(Error::Invalid(format!("not an HH:MM time: {time}")));
            }
        }
        if update.parallel == 0 {
            return Err(Error::Invalid("a queue needs at least 1 file at a time".into()));
        }
        let mut st = self.inner.state();
        let q = st.queues.get_mut(&update.id).ok_or(Error::NotFound(update.id))?;
        let updated = Queue { running: q.running, ..update };
        self.inner.store().save_queue(&updated)?;
        *q = updated.clone();
        self.inner.pump_locked(&mut st);
        Ok(updated)
    }

    pub fn start_queue(&self, id: i64) -> Result<()> {
        self.inner.start_queue(id)
    }

    /// Stops a queue; its running downloads go back to waiting in it.
    pub async fn stop_queue(&self, id: i64) -> Result<()> {
        self.inner.stop_queue(id).await
    }
}

impl Inner {
    /// Starts waiting downloads in running queues up to each queue's limit. A queue with
    /// nothing left to do stops, as in IDM.
    pub(super) fn pump_locked(self: &Arc<Self>, st: &mut State) {
        let queues: Vec<(i64, u32)> = st.queues.values().filter(|q| q.running).map(|q| (q.id, q.parallel)).collect();
        for (qid, parallel) in queues {
            let active = st.running.values().filter(|r| r.queue == Some(qid)).count();
            let mut waiting: Vec<&Download> = st
                .downloads
                .values()
                .filter(|d| d.queue == qid && d.status == Status::Waiting && !d.provisional && !st.running.contains_key(&d.id))
                .collect();
            waiting.sort_by_key(|d| (d.position, d.id));
            let next: Vec<i64> =
                waiting.iter().take((parallel as usize).saturating_sub(active)).map(|d| d.id).collect();
            if active == 0 && next.is_empty() {
                let q = st.queues.get_mut(&qid).expect("listed above");
                q.running = false;
                if let Err(e) = self.store().save_queue(q) {
                    eprintln!("rdm: could not save queue {qid}: {e}");
                }
                continue;
            }
            for id in next {
                self.start_locked(st, id, Some(qid));
            }
        }
    }

    /// Starts a queue (by hand or by its schedule). Each of its files gets its retries
    /// afresh, and those that failed wait again, so a queue run after a stop, or on the
    /// next day, tries them again. Files that failed for good (the server said there is
    /// no file at their address) and paused files stay as they are; files still waiting
    /// for Download File Info's answer are left to it.
    pub(super) fn start_queue(self: &Arc<Self>, id: i64) -> Result<()> {
        let mut st = self.state();
        self.set_running(&mut st, id, true)?;
        let again = |d: &Download| d.status != Status::Complete && (d.status == Status::Failed || d.attempts > 0);
        for d in st.downloads.values_mut().filter(|d| d.queue == id && !d.provisional && !d.failed_for_good && again(d)) {
            if d.status == Status::Failed {
                d.status = Status::Waiting;
            }
            d.attempts = 0;
            self.persist(d);
        }
        self.pump_locked(&mut st);
        Ok(())
    }

    pub(super) async fn stop_queue(self: &Arc<Self>, id: i64) -> Result<()> {
        let started_by_queue: Vec<i64> = {
            let mut st = self.state();
            self.set_running(&mut st, id, false)?;
            st.running.iter().filter(|(_, r)| r.queue == Some(id)).map(|(id, _)| *id).collect()
        };
        for d in started_by_queue {
            self.stop_and_wait(d, AfterStop::Wait).await;
        }
        Ok(())
    }

    /// Marks queue `id` as running or not, once that is saved.
    fn set_running(&self, st: &mut State, id: i64, running: bool) -> Result<()> {
        let q = st.queues.get_mut(&id).ok_or(Error::NotFound(id))?;
        self.store().save_queue(&Queue { running, ..q.clone() })?;
        q.running = running;
        Ok(())
    }
}

/// The position after the last download in `queue`.
pub(super) fn next_position(downloads: &BTreeMap<i64, Download>, queue: i64) -> i64 {
    downloads.values().filter(|d| d.queue == queue).map(|d| d.position).max().unwrap_or(0) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::testing;
    use crate::MAIN_QUEUE;
    use crate::model::{Event, NewDownload};

    /// A queue change that cannot be saved is not made: not renamed, not started, not
    /// stopped (its download keeps running).
    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_queue_changes_change_nothing() {
        let s = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, db) = testing::open("unsaved-queues");
        let busy = m.new_queue("Busy").unwrap();
        let idle = m.new_queue("Idle").unwrap();
        // 1 MiB at 16 KiB/s keeps the queue running for the whole test.
        let url = s.url("/data?name=q.bin&rate=16384");
        m.add(NewDownload { url, queue: Some(busy.id), ..NewDownload::default() }).unwrap();
        m.start_queue(busy.id).unwrap();
        let before = format!("{:?}", m.queues());
        assert!(before.contains("running: true"), "{before}");

        testing::tables_away(&db, true);
        assert!(m.update_queue(Queue { name: "Renamed".into(), ..busy.clone() }).is_err());
        assert!(m.start_queue(idle.id).is_err());
        assert!(m.stop_queue(busy.id).await.is_err());
        assert_eq!(format!("{:?}", m.queues()), before);
        assert_eq!(m.list()[0].status, Status::Downloading);

        testing::tables_away(&db, false);
        m.stop_queue(busy.id).await.unwrap();
        assert!(!m.queues().iter().any(|q| q.running));
        assert_eq!(m.list()[0].status, Status::Waiting);
        m.shutdown().await;
    }

    /// A queue started again (by hand or by its schedule) tries its failed files again with
    /// their retries afresh, but not one that failed for good (the server does not have
    /// it), and gives a paused file its retries afresh without starting it; a move to a
    /// queue does the same. The fresh counts are saved.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_started_again_retries_its_failed_files() {
        let s = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, db) = testing::open("retries-afresh");
        // No retries: a failure leaves the count as it is, so a test of it does not race.
        let main = m.queues().remove(0);
        m.update_queue(Queue { retries: 0, ..main }).unwrap();
        let other = m.new_queue("Other").unwrap();
        // Does not match the server's hash: may come right another time.
        let wrong = "0".repeat(40);
        let url = s.url(&format!("/data?name=f.bin&header=x-bz-content-sha1&value={wrong}"));
        let failing = m.add(NewDownload { url, ..NewDownload::default() }).unwrap().id;
        let paused = m.add(NewDownload { url: s.url("/data?name=p.bin"), ..NewDownload::default() }).unwrap().id;
        let dead = m.add(NewDownload { url: s.url("/missing"), ..NewDownload::default() }).unwrap().id;
        m.pause(paused).await.unwrap();
        m.start_queue(MAIN_QUEUE).unwrap();
        failed(&m, failing).await;
        failed(&m, dead).await;
        assert!(m.get(dead).unwrap().failed_for_good);

        // As if both had used up their retries in an earlier run, in memory and on disk.
        let used_up = |id: i64| {
            m.inner.state().downloads.get_mut(&id).unwrap().attempts = 5;
            db.execute("UPDATE downloads SET attempts = 5 WHERE id = ?1", [id]).unwrap();
        };
        let saved = |id: i64| -> u32 { db.query_row("SELECT attempts FROM downloads WHERE id = ?1", [id], |r| r.get(0)).unwrap() };
        used_up(failing);
        used_up(paused);
        let mut events = m.subscribe();
        m.start_queue(MAIN_QUEUE).unwrap();
        failed(&m, failing).await;
        let started: Vec<i64> =
            std::iter::from_fn(|| events.try_recv().ok()).filter_map(|e| if let Event::Started(id) = e { Some(id) } else { None }).collect();
        assert_eq!(started, [failing], "only the file that may come right is tried again");
        assert_eq!((m.get(failing).unwrap().attempts, saved(failing)), (0, 0), "failed file");
        let d = m.get(dead).unwrap();
        let stored: bool = db.query_row("SELECT failed_for_good FROM downloads WHERE id = ?1", [dead], |r| r.get(0)).unwrap();
        assert_eq!((d.status, d.failed_for_good, stored), (Status::Failed, true, true), "dead link");
        let p = m.get(paused).unwrap();
        assert_eq!((p.status, p.attempts, saved(paused)), (Status::Paused, 0, 0), "paused file");

        used_up(paused);
        m.move_to_queue(&[paused], other.id).unwrap();
        assert_eq!((m.get(paused).unwrap().attempts, saved(paused)), (0, 0), "moved");
        m.shutdown().await;
    }

    async fn failed(m: &Manager, id: i64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while m.get(id).unwrap().status != Status::Failed {
            assert!(std::time::Instant::now() < deadline, "no failure");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
