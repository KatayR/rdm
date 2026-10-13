//! RDM's own update: a newer setup from the release page, checked against RDM's key and run.
//!
//! Looking, asking and installing are three steps. The plugin gives the pieces (the release
//! page's answer, the download with its signature check, the start of the setup); when each
//! happens is decided here, and the setup is started only after RDM has closed down as it
//! does on Exit, so that it finds none of RDM's programs running.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use manager::model::Status;
use serde::Serialize;
use tauri::async_runtime::block_on;
use tauri::{AppHandle, Manager as _, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{AppState, fatal, windows};

/// After the app's start, so the first look does not slow it.
const FIRST_LOOK: Duration = Duration::from_secs(10);
const LOOK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// After a look that failed (no network at start, say).
const LOOK_AGAIN: Duration = Duration::from_secs(60 * 60);
/// The release page's answer is a few hundred bytes; a connection that stalls is given up.
const LOOK_WITHIN: Duration = Duration::from_secs(30);
/// The whole setup (14 MB) has this long.
const FETCH_WITHIN: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct Updates {
    /// The newer version the last look found.
    found: Mutex<Option<Update>>,
    /// "Update now" is at work: there is one at a time.
    installing: AtomicBool,
    /// Of the setup being downloaded: bytes so far, and its size (0 while not known).
    received: AtomicU64,
    size: AtomicU64,
}

/// What the update window shows.
#[derive(Serialize)]
pub struct Found {
    version: String,
    current: String,
    notes: String,
    /// Downloads running now: they stop for the update and continue after it.
    running: usize,
    /// "Update now" was pressed and is at work (the window was closed and opened again).
    installing: bool,
}

fn describe(update: &Update, state: &AppState) -> Found {
    Found {
        version: update.version.clone(),
        current: update.current_version.clone(),
        notes: update.body.clone().unwrap_or_default(),
        running: state.manager.list().iter().filter(|d| d.status == Status::Downloading).count(),
        installing: state.updates.installing.load(Ordering::Relaxed),
    }
}

/// Whether this copy was put here by the setup, which leaves its uninstaller beside the
/// programs. A copy run from the build folder is not what a setup would replace. A Mac has
/// no setup: there an installed copy is one in an app bundle (`RDM.app/Contents/MacOS`).
pub(crate) fn installed() -> bool {
    let Ok(me) = std::env::current_exe() else { return false };
    if cfg!(target_os = "macos") {
        me.parent().is_some_and(|folder| folder.ends_with("Contents/MacOS"))
    } else {
        me.with_file_name("uninstall.exe").is_file()
    }
}

/// Asks the release page; what it found is kept for the window and the install.
async fn look(app: &AppHandle) -> Result<Option<Found>, String> {
    let closing = app.clone();
    let updater = app
        .updater_builder()
        .timeout(LOOK_WITHIN)
        // Runs when the setup is fetched, checked and written to disk, just before it is
        // started: from here RDM is closing, and the setup must find none of it at work.
        .on_before_exit(move || {
            tokio::task::block_in_place(|| block_on(crate::close_down(&closing)));
            closing.cleanup_before_exit();
        })
        .build()
        .map_err(|e| e.to_string())?;
    let mut update = updater.check().await.map_err(|e| e.to_string())?;
    if let Some(update) = &mut update {
        update.timeout = Some(FETCH_WITHIN);
    }
    let state = app.state::<AppState>();
    let found = update.as_ref().map(|update| describe(update, &state));
    // Not under an install at work: it holds its own copy, and the window describes it.
    if !state.updates.installing.load(Ordering::Relaxed) {
        *state.updates.found.lock().unwrap() = update;
    }
    Ok(found)
}

/// Looks once a day while the option is on, and opens the update window for a version not
/// offered yet since the app started ("Later" lasts until the next start or the next version).
pub async fn keep_looking(app: AppHandle) {
    if !installed() {
        return;
    }
    tokio::time::sleep(FIRST_LOOK).await;
    let mut offered: Option<String> = None;
    loop {
        let mut wait = LOOK_EVERY;
        if app.state::<AppState>().manager.settings().check_for_updates {
            match look(&app).await {
                Ok(Some(found)) if offered.as_deref() != Some(found.version.as_str()) => {
                    offered = Some(found.version);
                    windows::open_update(&app);
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("rdm-app: looking for a new version: {e}");
                    wait = LOOK_AGAIN;
                }
            }
        }
        tokio::time::sleep(wait).await;
    }
}

/// The setups earlier updates left in the temporary folder (the plugin keeps them: the
/// setup is still running when the app that fetched it ends).
pub fn clear_old_setups(app: &AppHandle) {
    let start = format!("{}-", app.package_info().name);
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // "<name>-<version>-updater-<random>", as the plugin names them.
        if name.starts_with(&start) && name.contains("-updater-") && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The version of this program (Options' About).
#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// About's "Check for updates".
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<Option<Found>, String> {
    if !installed() {
        return Err("This copy of RDM was not installed (it runs from where it was built), so it does not update itself.".into());
    }
    look(&app).await
}

#[tauri::command]
pub async fn open_update(app: AppHandle) {
    windows::open_update(&app);
}

/// For the update window: what was found, with the downloads running now.
#[tauri::command]
pub fn update_found(state: State<AppState>) -> Option<Found> {
    state.updates.found.lock().unwrap().as_ref().map(|update| describe(update, &state))
}

/// Bytes of the setup fetched so far, and its size (0 while not known).
#[tauri::command]
pub fn update_progress(state: State<AppState>) -> (u64, u64) {
    (state.updates.received.load(Ordering::Relaxed), state.updates.size.load(Ordering::Relaxed))
}

/// "Update now": fetches the setup and checks its signature, then hands over to it: RDM
/// closes down (`look`'s `on_before_exit`), the setup puts the new programs in place and
/// starts RDM again. An error before RDM began to close leaves it running as it was.
/// On a Mac the update is no setup but the new app itself (`hand_over`).
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let update = state.updates.found.lock().unwrap().clone().ok_or("no newer version was found")?;
    if state.updates.installing.swap(true, Ordering::Relaxed) {
        return Err("the update is already being fetched".into());
    }
    let (received, size) = (&state.updates.received, &state.updates.size);
    received.store(0, Ordering::Relaxed);
    size.store(0, Ordering::Relaxed);
    let chunk = |bytes: usize, of: Option<u64>| {
        received.fetch_add(bytes as u64, Ordering::Relaxed);
        size.store(of.unwrap_or(0), Ordering::Relaxed);
    };
    let failed = match update.download(chunk, || {}).await {
        Ok(setup) => hand_over(&app, &update, setup).await,
        Err(e) => e.to_string(),
    };
    if app.state::<AppState>().closed_down() {
        fatal(&format!("The update could not be started ({failed}).\n\nRDM closes now; start it again."));
    }
    state.updates.installing.store(false, Ordering::Relaxed);
    Err(failed)
}

/// Writes the setup to disk, closes RDM down, starts the setup and ends this program: it
/// returns only when one of these did not work, with what went wrong.
#[cfg(not(target_os = "macos"))]
async fn hand_over(_app: &AppHandle, update: &Update, setup: Vec<u8>) -> String {
    update.install(setup).err().map_or_else(|| "the setup did not start".to_owned(), |e| e.to_string())
}

/// On a Mac: closes RDM down, puts the new app in the place of this one and starts it. The
/// programs of the old one that still run (the browsers' helper, say) keep their files
/// until they end. The new app tells the browsers about itself as every start does
/// (`tell_browsers`). It returns only when the new app could not be put in place.
#[cfg(target_os = "macos")]
async fn hand_over(app: &AppHandle, update: &Update, new_app: Vec<u8>) -> String {
    crate::close_down(app).await;
    match update.install(new_app) {
        Ok(()) => app.restart(),
        Err(e) => e.to_string(),
    }
}
