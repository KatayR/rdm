//! Download manager: the list, categories, queues, scheduler and speed limits, run by one
//! background process that clients reach over a local socket.

pub mod caught;
mod categories;
mod filecheck;
pub mod ipc;
mod manager;
pub mod media;
pub mod model;
pub mod schedule;
mod store;
pub mod tools;
pub mod video;

use std::path::PathBuf;

pub use manager::{Config, Manager};
pub use store::MAIN_QUEUE;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("no download or queue with id {0}")]
    NotFound(i64),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Download(#[from] engine::Error),
    /// The server's first answer for a plain download: there is no file at its address
    /// (see `engine::Error::is_gone`). A refusal later in the download is a `Download`.
    #[error("{0}")]
    Gone(engine::Error),
    #[error("{0}")]
    Tool(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Data folder: `RDM_HOME` if set, else `%APPDATA%\rdm` on Windows and
/// `~/Library/Application Support/rdm` on macOS.
pub fn home() -> PathBuf {
    std::env::var_os("RDM_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("rdm")))
        .unwrap_or_else(|| PathBuf::from(".rdm"))
}
