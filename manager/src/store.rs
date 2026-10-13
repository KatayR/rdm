//! SQLite storage for downloads, categories, queues and settings.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::categories;
use crate::model::{Category, Download, Playlist, Queue, Schedule, Settings, Status};
use crate::{Error, Result};

pub const MAIN_QUEUE: i64 = 1;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database; a new one gets IDM's default categories, the main
    /// queue and default settings with `downloads_dir`.
    ///
    /// A database written by a newer program is refused before anything in it is touched:
    /// this program would not know its newer columns, and would save without them.
    pub fn open(path: &Path, downloads_dir: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        let form: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if form > STEPS.len() as i64 {
            return Err(Error::Invalid(format!(
                "the list in {} was written by a newer RDM (its form is {form}, this RDM knows up to {}). Install the newer RDM again; the list was not touched.",
                path.display(),
                STEPS.len()
            )));
        }
        let existed: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'downloads')", [], |r| r.get(0))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS downloads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL, headers TEXT NOT NULL, file_name TEXT, dir TEXT, category TEXT,
                queue INTEGER NOT NULL, position INTEGER NOT NULL, size INTEGER,
                downloaded INTEGER NOT NULL, status TEXT NOT NULL, connections INTEGER,
                speed_limit INTEGER, added INTEGER NOT NULL, last_try INTEGER,
                attempts INTEGER NOT NULL, error TEXT, path TEXT);
             CREATE TABLE IF NOT EXISTS categories (
                position INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                extensions TEXT NOT NULL, dir TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS queues (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
                parallel INTEGER NOT NULL, retries INTEGER NOT NULL,
                start TEXT, stop TEXT, days INTEGER NOT NULL, running INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS playlists (
                download INTEGER NOT NULL, position INTEGER NOT NULL, url TEXT NOT NULL,
                text TEXT NOT NULL, PRIMARY KEY (download, position));",
        )?;
        migrate(&conn, existed.then_some(path))?;
        let store = Store { conn };
        if store.settings_row()?.is_none() {
            store.seed(downloads_dir)?;
        }
        Ok(store)
    }

    fn seed(&self, downloads_dir: &Path) -> Result<()> {
        for (i, c) in categories::defaults(downloads_dir).iter().enumerate() {
            self.conn.execute(
                "INSERT INTO categories (position, name, extensions, dir) VALUES (?1, ?2, ?3, ?4)",
                params![i as i64, c.name, c.extensions.join(" "), path_text(&c.dir)],
            )?;
        }
        let mut main = Queue {
            id: 0,
            name: "Main download queue".into(),
            parallel: 1,
            retries: 10,
            schedule: Schedule::default(),
            running: false,
        };
        self.insert_queue(&mut main)?;
        debug_assert_eq!(main.id, MAIN_QUEUE);
        self.save_settings(&Settings {
            connections: 8,
            speed_limit: 0,
            downloads_dir: downloads_dir.to_owned(),
            ask_before_download: true,
            show_progress: true,
            show_complete: true,
            exclude_types: Vec::new(),
            speed_limit_value: 0,
            auto_update_tools: true,
            check_for_updates: true,
        })
    }

    pub fn downloads(&self) -> Result<Vec<Download>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {DOWNLOAD_COLUMNS} FROM downloads ORDER BY id"))?;
        let rows = stmt.query_map([], download_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores a new download and sets its id; with it, in their order, the playlists it
    /// was picked from (`NewDownload::playlists`). Both or neither.
    pub fn insert(&self, d: &mut Download, playlists: &[Playlist]) -> Result<()> {
        let all = self.conn.unchecked_transaction()?;
        self.conn.execute(
            "INSERT INTO downloads (url, headers, queue, position, downloaded, status, added, attempts)
             VALUES (?1, '[]', ?2, ?3, 0, 'waiting', ?4, 0)",
            params![d.url, d.queue, d.position, d.added],
        )?;
        d.id = self.conn.last_insert_rowid();
        self.save(d)?;
        for (position, p) in playlists.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO playlists (download, position, url, text) VALUES (?1, ?2, ?3, ?4)",
                params![d.id, position as i64, p.url, p.text],
            )?;
        }
        Ok(all.commit()?)
    }

    pub fn save(&self, d: &Download) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE downloads SET url = ?2, headers = ?3, file_name = ?4, dir = ?5, category = ?6,
                queue = ?7, position = ?8, size = ?9, downloaded = ?10, status = ?11,
                connections = ?12, speed_limit = ?13, added = ?14, last_try = ?15,
                attempts = ?16, error = ?17, path = ?18, description = ?19, video = ?20,
                size_estimated = ?21, checked = ?22, file_check = ?23, provisional = ?24,
                failed_for_good = ?25, placed = ?26
             WHERE id = ?1",
            params![
                d.id,
                d.url,
                serde_json::to_string(&d.headers)?,
                d.file_name,
                d.dir.as_deref().map(path_text),
                d.category,
                d.queue,
                d.position,
                d.size.map(|s| s as i64),
                d.downloaded as i64,
                d.status.as_str(),
                d.connections,
                d.speed_limit.map(|s| s as i64),
                d.added,
                d.last_try,
                d.attempts,
                d.error,
                d.path.as_deref().map(path_text),
                d.description,
                d.video,
                d.size_estimated,
                d.checked,
                d.file_check.as_ref().map(serde_json::to_string).transpose()?,
                d.provisional,
                d.failed_for_good,
                d.placed,
            ],
        )?;
        if changed == 0 {
            return Err(Error::NotFound(d.id));
        }
        Ok(())
    }

    /// Deletes a download, and its playlists with it.
    pub fn delete(&self, id: i64) -> Result<()> {
        let both = self.conn.unchecked_transaction()?;
        self.conn.execute("DELETE FROM playlists WHERE download = ?1", [id])?;
        self.conn.execute("DELETE FROM downloads WHERE id = ?1", [id])?;
        Ok(both.commit()?)
    }

    /// The playlists a video download was picked from, in their order.
    pub fn playlists(&self, id: i64) -> Result<Vec<Playlist>> {
        let mut stmt = self.conn.prepare("SELECT url, text FROM playlists WHERE download = ?1 ORDER BY position")?;
        let rows = stmt.query_map([id], |r| Ok(Playlist { url: r.get(0)?, text: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn categories(&self) -> Result<Vec<Category>> {
        let mut stmt = self.conn.prepare("SELECT name, extensions, dir FROM categories ORDER BY position")?;
        let rows = stmt.query_map([], |r| {
            Ok(Category {
                name: r.get(0)?,
                extensions: r.get::<_, String>(1)?.split_whitespace().map(str::to_owned).collect(),
                dir: PathBuf::from(r.get::<_, String>(2)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_category_dir(&self, name: &str, dir: &Path) -> Result<()> {
        self.conn.execute("UPDATE categories SET dir = ?2 WHERE name = ?1", params![name, path_text(dir)])?;
        Ok(())
    }

    pub fn queues(&self) -> Result<Vec<Queue>> {
        let mut stmt =
            self.conn.prepare("SELECT id, name, parallel, retries, start, stop, days, running FROM queues ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Queue {
                id: r.get(0)?,
                name: r.get(1)?,
                parallel: r.get(2)?,
                retries: r.get(3)?,
                schedule: Schedule { start: r.get(4)?, stop: r.get(5)?, days: r.get(6)? },
                running: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores a new queue (in one statement, so a failure leaves no half-made queue) and
    /// sets its id.
    pub fn insert_queue(&self, q: &mut Queue) -> Result<()> {
        self.conn.execute(
            "INSERT INTO queues (name, parallel, retries, start, stop, days, running)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![q.name, q.parallel, q.retries, q.schedule.start, q.schedule.stop, q.schedule.days, q.running],
        )?;
        q.id = self.conn.last_insert_rowid();
        Ok(())
    }

    pub fn save_queue(&self, q: &Queue) -> Result<()> {
        self.conn.execute(
            "UPDATE queues SET name = ?2, parallel = ?3, retries = ?4, start = ?5, stop = ?6,
                days = ?7, running = ?8 WHERE id = ?1",
            params![q.id, q.name, q.parallel, q.retries, q.schedule.start, q.schedule.stop, q.schedule.days, q.running],
        )?;
        Ok(())
    }

    pub fn settings(&self) -> Result<Settings> {
        let json = self.settings_row()?.ok_or(Error::Invalid("settings missing".into()))?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('settings', ?1)",
            [serde_json::to_string(s)?],
        )?;
        Ok(())
    }

    fn settings_row(&self) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = 'settings'", [], |r| r.get(0)).optional()?)
    }
}

const DOWNLOAD_COLUMNS: &str = "id, url, headers, file_name, dir, category, queue, position, size, downloaded, \
     status, connections, speed_limit, added, last_try, attempts, error, path, description, video, size_estimated, checked, file_check, provisional, \
     failed_for_good, placed";

fn download_from_row(r: &Row) -> rusqlite::Result<Download> {
    let json_err = |e: serde_json::Error| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, e.into());
    let status: String = r.get(10)?;
    Ok(Download {
        id: r.get(0)?,
        url: r.get(1)?,
        headers: serde_json::from_str(&r.get::<_, String>(2)?).map_err(json_err)?,
        file_name: r.get(3)?,
        dir: r.get::<_, Option<String>>(4)?.map(PathBuf::from),
        category: r.get(5)?,
        queue: r.get(6)?,
        position: r.get(7)?,
        size: r.get::<_, Option<i64>>(8)?.map(|s| s as u64),
        downloaded: r.get::<_, i64>(9)? as u64,
        status: Status::parse(&status).unwrap_or(Status::Failed),
        connections: r.get(11)?,
        speed_limit: r.get::<_, Option<i64>>(12)?.map(|s| s as u64),
        added: r.get(13)?,
        last_try: r.get(14)?,
        attempts: r.get(15)?,
        error: r.get(16)?,
        path: r.get::<_, Option<String>>(17)?.map(PathBuf::from),
        description: r.get(18)?,
        video: r.get(19)?,
        size_estimated: r.get(20)?,
        checked: r.get(21)?,
        file_check: r.get::<_, Option<String>>(22)?.map(|t| serde_json::from_str(&t)).transpose().map_err(json_err)?,
        checking: false,
        provisional: r.get(23)?,
        failed_for_good: r.get(24)?,
        placed: r.get(25)?,
        resumable: None,
        new_address: false,
        speed: 0,
        active_connections: 0,
        segments: Vec::new(),
        pieces: None,
    })
}

/// Schema changes after the first version, applied in order; PRAGMA user_version
/// records how many ran.
const STEPS: [&str; 8] = [
    "ALTER TABLE downloads ADD COLUMN description TEXT",
    "ALTER TABLE downloads ADD COLUMN video TEXT",
    "ALTER TABLE downloads ADD COLUMN size_estimated INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE downloads ADD COLUMN checked TEXT",
    "ALTER TABLE downloads ADD COLUMN file_check TEXT",
    "ALTER TABLE downloads ADD COLUMN provisional INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE downloads ADD COLUMN failed_for_good INTEGER NOT NULL DEFAULT 0",
    // Whether a download was given its name was not recorded before: one that was ever
    // started is taken as placed, as every download was until then.
    "ALTER TABLE downloads ADD COLUMN placed INTEGER NOT NULL DEFAULT 0;
     UPDATE downloads SET placed = 1 WHERE last_try IS NOT NULL",
];

/// Where the database is copied to before a newer program changes its form: what the
/// program before the update wrote, should the update have to be taken back.
pub fn before_update(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.before-update", db.display()))
}

/// Brings the database to this program's form. `existing` is its file when it held a list
/// before this opening: that list is copied (`before_update`, the last one only) before
/// the first change.
fn migrate(conn: &Connection, existing: Option<&Path>) -> Result<()> {
    let done: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let pending = STEPS.get(done as usize..).unwrap_or_default();
    if pending.is_empty() {
        return Ok(());
    }
    if let Some(db) = existing {
        let copy = before_update(db);
        let not_kept = |e: &dyn std::fmt::Display| Error::Invalid(format!("the list could not be copied to {} before its form is changed: {e}", copy.display()));
        match std::fs::remove_file(&copy) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(not_kept(&e)),
            _ => {}
        }
        conn.execute("VACUUM INTO ?1", [path_text(&copy)]).map_err(|e| not_kept(&e))?;
    }
    // All or none. A step made but not counted would be tried again, and fail; and a list
    // left between two forms is in none a program ever wrote, the copy of the next try
    // with it.
    let all = conn.unchecked_transaction()?;
    for step in pending {
        conn.execute_batch(step)?;
    }
    conn.pragma_update(None, "user_version", STEPS.len() as i64)?;
    all.commit()?;
    Ok(())
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileCheck;

    /// A database path in a folder of its own, removed when the guard is dropped.
    fn temp_db(name: &str) -> (testserver::Folder, PathBuf) {
        let dir = testserver::Folder::new("store");
        let path = dir.path().join(name);
        (dir, path)
    }

    #[test]
    fn first_version_databases_are_migrated() {
        let (_dir, path) = temp_db("v0.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE downloads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL, headers TEXT NOT NULL, file_name TEXT, dir TEXT, category TEXT,
                queue INTEGER NOT NULL, position INTEGER NOT NULL, size INTEGER,
                downloaded INTEGER NOT NULL, status TEXT NOT NULL, connections INTEGER,
                speed_limit INTEGER, added INTEGER NOT NULL, last_try INTEGER,
                attempts INTEGER NOT NULL, error TEXT, path TEXT);
             INSERT INTO downloads (url, headers, queue, position, downloaded, status, added, attempts)
                VALUES ('https://x.org/a.zip', '[]', 1, 1, 5, 'complete', 1, 0);
             INSERT INTO downloads (url, headers, queue, position, downloaded, status, added, attempts, last_try)
                VALUES ('https://x.org/b.zip', '[]', 1, 2, 5, 'paused', 1, 0, 2);",
        )
        .unwrap();
        drop(conn);
        let store = Store::open(&path, Path::new("C:/dl")).unwrap();
        let all = store.downloads().unwrap();
        assert_eq!((all.len(), all[0].downloaded, all[0].description.clone()), (2, 5, None));
        // What the later versions added, as an old download must come out: above all not
        // provisional (those are deleted with their files when the manager opens).
        let old = &all[0];
        assert_eq!((old.size_estimated, old.provisional, old.failed_for_good), (false, false, false));
        assert_eq!((old.video.clone(), old.checked.clone(), old.file_check.clone()), (None, None, None));
        // Given its name, as far as an old database knows: started at least once.
        assert_eq!((old.placed, all[1].placed), (false, true));
        drop(store);

        // The list as it was is kept beside it: the old form, with both downloads.
        let copy = before_update(&path);
        let kept = Connection::open(&copy).unwrap();
        let form: i64 = kept.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        let rows: i64 = kept.query_row("SELECT count(*) FROM downloads", [], |r| r.get(0)).unwrap();
        let columns: i64 = kept.query_row("SELECT count(*) FROM pragma_table_info('downloads')", [], |r| r.get(0)).unwrap();
        assert_eq!((form, rows, columns), (0, 2, 18));
        drop(kept);
        // Opened again there is nothing to change, so nothing is copied.
        std::fs::remove_file(&copy).unwrap();
        Store::open(&path, Path::new("C:/dl")).unwrap();
        assert!(!copy.exists());
    }

    /// A new database is no list to keep a copy of.
    #[test]
    fn a_new_database_leaves_no_copy() {
        let (_dir, path) = temp_db("new.db");
        drop(Store::open(&path, Path::new("C:/dl")).unwrap());
        assert!(!before_update(&path).exists());
    }

    /// A database from a newer program is refused, and not a byte of it changes.
    #[test]
    fn a_newer_database_is_refused_untouched() {
        let (_dir, path) = temp_db("newer.db");
        let store = Store::open(&path, Path::new("C:/dl")).unwrap();
        store.conn.pragma_update(None, "user_version", STEPS.len() as i64 + 1).unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let files = |dir: &Path| std::fs::read_dir(dir).unwrap().count();
        let beside = files(path.parent().unwrap());

        let Err(refused) = Store::open(&path, Path::new("C:/dl")) else { panic!("a newer database was opened") };
        assert!(matches!(&refused, Error::Invalid(text) if text.contains("newer RDM") && text.contains("not touched")), "{refused}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(files(path.parent().unwrap()), beside, "something was written next to it");
    }

    #[test]
    fn new_database_is_seeded_and_round_trips() {
        let (_dir, path) = temp_db("seed.db");
        let store = Store::open(&path, Path::new("C:/dl")).unwrap();
        assert_eq!(store.categories().unwrap().len(), 6);
        let queues = store.queues().unwrap();
        assert_eq!((queues.len(), queues[0].id, queues[0].parallel), (1, MAIN_QUEUE, 1));
        assert_eq!(store.settings().unwrap().connections, 8);

        let mut other = Queue { id: 0, name: "Other".into(), parallel: 2, retries: 3, schedule: Schedule::default(), running: false };
        store.insert_queue(&mut other).unwrap();
        // Every stored field with a value of its own, so one read from another's column
        // shows. What is not stored (speeds, segments and so on) is as a fresh load has it.
        let mut full = Download {
            id: 0,
            url: "https://x.org/a.zip".into(),
            headers: vec![("Referer".into(), "https://x.org/".into())],
            file_name: Some("a.zip".into()),
            dir: Some(PathBuf::from("C:/dl/somewhere")),
            category: Some("Compressed".into()),
            queue: other.id,
            position: 7,
            size: Some(5_000_000_000),
            size_estimated: true,
            downloaded: 42,
            status: Status::Paused,
            connections: Some(4),
            speed_limit: Some(65_536),
            added: 1_791_000_000,
            last_try: Some(1_791_000_100),
            attempts: 3,
            error: Some("no answer".into()),
            failed_for_good: false,
            path: Some(PathBuf::from("C:/dl/somewhere/a.zip")),
            description: Some("for testing".into()),
            video: Some("299+140".into()),
            checked: Some("SHA-256 from the server (Repr-Digest)".into()),
            file_check: Some(FileCheck { ok: false, text: "a.zip is damaged".into() }),
            checking: false,
            provisional: false,
            placed: false,
            resumable: None,
            new_address: false,
            speed: 0,
            active_connections: 0,
            segments: Vec::new(),
            pieces: None,
        };
        // The same with nothing optional, and each yes / no field true in one download only,
        // so two of them changing places show too.
        let bare = Download {
            url: "https://x.org/b.bin".into(),
            headers: Vec::new(),
            file_name: None,
            dir: None,
            category: None,
            queue: MAIN_QUEUE,
            position: 1,
            size: None,
            size_estimated: false,
            downloaded: 9,
            status: Status::Failed,
            connections: None,
            speed_limit: None,
            last_try: None,
            attempts: 2,
            error: None,
            path: None,
            description: None,
            video: None,
            checked: None,
            file_check: None,
            ..full.clone()
        };
        let mut unanswered = Download { provisional: true, ..bare.clone() };
        let mut dead = Download { failed_for_good: true, ..bare.clone() };
        let mut given = Download { placed: true, ..bare };
        let playlist = |url: &str, text: &str| Playlist { url: url.into(), text: text.into() };
        let picked_from = [playlist("https://x.org/master.m3u8", "#EXTM3U\nmaster"), playlist("blob:https://x.org/1-2", "#EXTM3U\nçok")];
        store.insert(&mut full, &picked_from).unwrap();
        let first = full.id;
        assert_eq!(store.playlists(first).unwrap(), picked_from, "in their order");
        store.delete(first).unwrap();
        assert!(store.playlists(first).unwrap().is_empty(), "its playlists go with a download");
        // A download whose playlists cannot be saved is not saved either.
        store.conn.execute_batch("ALTER TABLE playlists RENAME TO playlists_away").unwrap();
        assert!(store.insert(&mut full.clone(), &picked_from).is_err());
        store.conn.execute_batch("ALTER TABLE playlists_away RENAME TO playlists").unwrap();
        assert!(store.downloads().unwrap().is_empty());
        store.insert(&mut full, &picked_from[..1]).unwrap();
        assert!(full.id > first, "ids are never reused");
        store.insert(&mut unanswered, &[]).unwrap();
        store.insert(&mut dead, &[]).unwrap();
        store.insert(&mut given, &[]).unwrap();
        // What the first write of a new download sets is saved again when it changes.
        (dead.url, dead.queue, dead.position, dead.added) = ("https://y.org/c.bin".into(), other.id, 4, 1_791_000_005);
        store.save(&dead).unwrap();
        drop(store);

        let store = Store::open(&path, Path::new("ignored on reopen")).unwrap();
        let back = store.downloads().unwrap();
        assert_eq!(store.playlists(full.id).unwrap(), picked_from[..1]);
        assert!(store.playlists(dead.id).unwrap().is_empty());
        assert_eq!(format!("{back:#?}"), format!("{:#?}", [full, unanswered, dead, given]));
        assert_eq!(store.categories().unwrap().len(), 6, "seeded only once");
    }
}
