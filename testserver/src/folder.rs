//! Folders for tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A test's own empty folder under the system temp folder, with a random name so test runs
/// at once never meet; removed when dropped. A manager's background tasks can still hold its
/// database for a moment after shutdown, so removing is retried for a few seconds.
pub struct Folder(PathBuf);

impl Folder {
    /// A new folder whose name starts with `rdm-<test>-`.
    pub fn new(test: &str) -> Folder {
        let dir = tempfile::Builder::new().prefix(&format!("rdm-{test}-")).tempdir().expect("a temp folder");
        Folder(dir.keep())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        for _ in 0..50 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("could not remove {}", self.0.display());
    }
}
