//! Settings, speed limits and categories.

use std::path::PathBuf;

use super::Manager;
use crate::categories;
use crate::model::{Category, Settings};
use crate::{Error, Result};

impl Manager {
    pub fn settings(&self) -> Settings {
        self.inner.state().settings.clone()
    }

    /// Bytes per second for all downloads together; 0 lifts the limit. Applies at once.
    pub fn set_speed_limit(&self, rate: u64) -> Result<()> {
        self.change_settings(|s| s.speed_limit = rate)
    }

    /// Replaces the settings; the speed limit applies at once.
    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        self.change_settings(|s| *s = settings)
    }

    /// Default connections for downloads that do not set their own.
    pub fn set_connections(&self, connections: u16) -> Result<()> {
        self.change_settings(|s| s.connections = connections)
    }

    /// Changes the settings under one lock. Nothing changes, the speed limit included,
    /// unless the new settings were saved.
    fn change_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut st = self.inner.state();
        let mut settings = st.settings.clone();
        change(&mut settings);
        check_connections(settings.connections)?;
        self.inner.store().save_settings(&settings)?;
        self.inner.global.set_rate(settings.speed_limit);
        st.settings = settings;
        Ok(())
    }

    /// A download's own limit; 0 lifts it. Applies at once if it is running.
    pub fn set_download_limit(&self, id: i64, rate: u64) -> Result<()> {
        let mut st = self.inner.state();
        let d = st.downloads.get_mut(&id).ok_or(Error::NotFound(id))?;
        d.speed_limit = (rate > 0).then_some(rate);
        self.inner.persist(d);
        if let Some(run) = st.running.get(&id) {
            run.limiter.set_rate(rate);
        }
        Ok(())
    }

    /// The category a file of this name goes to.
    pub fn category_for(&self, file_name: &str) -> Option<Category> {
        categories::for_file(&self.inner.state().categories, file_name).cloned()
    }

    pub fn categories(&self) -> Vec<Category> {
        self.inner.state().categories.clone()
    }

    /// Sets a category's folder; General's folder is where uncategorized files go. Downloads
    /// that already have a folder keep it.
    pub fn set_category_dir(&self, name: &str, dir: PathBuf) -> Result<Category> {
        if !dir.is_absolute() {
            return Err(Error::Invalid(format!("the folder must be an absolute path: {}", dir.display())));
        }
        let mut st = self.inner.state();
        let c = st
            .categories
            .iter_mut()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::Invalid(format!("no category named {name}")))?;
        self.inner.store().save_category_dir(&c.name, &dir)?;
        c.dir = dir;
        Ok(c.clone())
    }
}

/// Downloads use 1 to 32 connections.
fn check_connections(connections: u16) -> Result<()> {
    if !(1..=32).contains(&connections) {
        return Err(Error::Invalid("connections must be 1 to 32".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::testing;

    /// A change that cannot be saved is not made: not in the settings, not in the speed
    /// limiter, not in a category's folder.
    #[tokio::test(flavor = "multi_thread")]
    async fn unsaved_settings_change_nothing() {
        let (dir, m, db) = testing::open("unsaved-settings");
        let before = (format!("{:?}", m.settings()), format!("{:?}", m.categories()), m.inner.global.rate());
        let elsewhere = dir.path().join("elsewhere");
        testing::tables_away(&db, true);
        assert!(m.set_speed_limit(1000).is_err());
        assert!(m.set_connections(3).is_err());
        assert!(m.update_settings(Settings { speed_limit: 2000, connections: 4, ..m.settings() }).is_err());
        assert!(m.set_category_dir("Video", elsewhere.clone()).is_err());
        assert_eq!((format!("{:?}", m.settings()), format!("{:?}", m.categories()), m.inner.global.rate()), before);

        testing::tables_away(&db, false);
        m.set_speed_limit(1000).unwrap();
        m.set_category_dir("Video", elsewhere.clone()).unwrap();
        assert_eq!((m.settings().speed_limit, m.inner.global.rate()), (1000, 1000));
        assert_eq!(m.category_for("a.mp4").unwrap().dir, elsewhere);
        m.shutdown().await;
    }
}
