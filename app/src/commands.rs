//! What the windows call. Errors come back as text for the window to show.

use std::path::PathBuf;

use manager::model::{Category, Download, HashCheck, NewDownload, Queue, Settings, ToolInfo, ToolUpdate, VideoPage};
use manager::caught::{self, Source};
use manager::video::VideoInfo;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::windows;
use crate::AppState;

type Answer<T> = Result<T, String>;

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// A new download from the socket or Add URL: the Download File Info window, or straight
/// in when the person turned that window off.
pub fn ask(app: &AppHandle, new: NewDownload) {
    let state = tauri::Manager::state::<AppState>(app);
    if !state.manager.settings().ask_before_download {
        if let Err(e) = state.manager.add(NewDownload { ask: false, ..new }) {
            eprintln!("rdm-app: {e}");
        }
        return;
    }
    let token = state.add_pending(new.clone());
    // The address is in the list already: the window says so, and nothing starts before
    // the answer.
    if let Some(existing) = state.manager.find_same_url(&new.url) {
        state.duplicates.lock().unwrap().insert(token, existing.id);
        windows::open_info(app, token);
        return;
    }
    // As IDM: the download starts while the window asks, so it is under way by the answer.
    let url = new.url.clone();
    match state.manager.add(NewDownload { provisional: true, ask: false, ..new }) {
        Ok(d) => {
            state.early.lock().unwrap().insert(token, d.id);
        }
        Err(e) => eprintln!("rdm-app: could not start {url} early: {e}"),
    }
    windows::open_info(app, token);
}

/// The download already in the list with the address of Download File Info `token`.
#[tauri::command]
pub fn duplicate_of(state: State<AppState>, token: u64) -> Option<Download> {
    let id = *state.duplicates.lock().unwrap().get(&token)?;
    state.manager.get(id).ok()
}

/// "Continue it": the download already in the list goes on, and the new one is dropped.
#[tauri::command]
pub fn continue_duplicate(state: State<AppState>, token: u64) -> Answer<()> {
    let id = *state.duplicates.lock().unwrap().get(&token).ok_or("this download was already answered")?;
    state.manager.resume(id).map_err(text)?;
    state.duplicates.lock().unwrap().remove(&token);
    state.pending.lock().unwrap().remove(&token);
    Ok(())
}

/// An unfinished download of the same file (name and size) as the one that started early
/// for Download File Info `token`: its link may have expired, and this is a fresh one.
#[tauri::command]
pub fn same_file(state: State<AppState>, token: u64) -> Option<Download> {
    let early = *state.early.lock().unwrap().get(&token)?;
    let d = state.manager.get(early).ok()?;
    state.manager.find_same_file(d.file_name.as_deref()?, d.size?)
}

/// "Use this address for it": the early download goes (with its files), and the unfinished
/// download `id` continues from this window's address with the browser's headers.
#[tauri::command]
pub async fn refresh_address(state: State<'_, AppState>, token: u64, id: i64) -> Answer<()> {
    let new = state.pending.lock().unwrap().get(&token).cloned().ok_or("this download was already answered")?;
    let early = state.early.lock().unwrap().remove(&token);
    if let Some(early) = early {
        state.manager.remove(early, true).await.map_err(text)?;
    }
    state.manager.refresh_address(id, new.url, new.headers).await.map_err(text)?;
    state.pending.lock().unwrap().remove(&token);
    Ok(())
}

/// A download's own window: progress while unfinished, Properties once done.
#[tauri::command]
pub async fn show_download(app: AppHandle, state: State<'_, AppState>, id: i64) -> Answer<()> {
    let d = state.manager.get(id).map_err(text)?;
    if d.status == manager::model::Status::Complete {
        windows::open_properties(&app, id);
    } else {
        windows::open_progress(&app, id);
    }
    Ok(())
}

#[tauri::command]
pub async fn open_properties(app: AppHandle, id: i64) {
    windows::open_properties(&app, id);
}

/// Delete in the main window: the question, in a window of its own.
#[tauri::command]
pub async fn open_delete(app: AppHandle, ids: Vec<i64>) {
    windows::open_delete(&app, &ids);
}

/// Properties' OK: the description (empty clears it).
#[tauri::command]
pub fn set_description(state: State<AppState>, id: i64, description: String) -> Answer<()> {
    state.manager.set_description(id, Some(description)).map_err(text)
}

/// The download that started for Download File Info `token`, if one could be started.
#[tauri::command]
pub fn early_download(state: State<AppState>, token: u64) -> Option<i64> {
    state.early.lock().unwrap().get(&token).copied()
}

/// A video page: the Download Video window lists its qualities, and those of the streams
/// its tab played, and the person picks one.
pub fn ask_video(app: &AppHandle, page: VideoPage) {
    let state = tauri::Manager::state::<AppState>(app);
    let new = NewDownload { url: page.url.clone(), headers: page.headers.clone(), start: true, ..NewDownload::default() };
    let token = state.add_pending(new);
    state.videos.lock().unwrap().insert(token, page);
    windows::open_video(app, token);
}

/// The page of Download Video `token`. What its tab played comes sorted from
/// `video_sources`.
#[tauri::command]
pub fn video_page(state: State<AppState>, token: u64) -> Answer<VideoPage> {
    let videos = state.videos.lock().unwrap();
    let page = videos.get(&token).ok_or("this download was already answered")?;
    Ok(VideoPage { url: page.url.clone(), title: page.title.clone(), headers: page.headers.clone(), caught: Vec::new(), player: None })
}

/// The quality list of Download Video `token` from what the page's own player was given,
/// when the browser sent that: shown at once, until yt-dlp's list (`video_choices`) is in.
#[tauri::command]
pub fn video_ahead(state: State<AppState>, token: u64) -> Answer<Option<VideoInfo>> {
    let videos = state.videos.lock().unwrap();
    let page = videos.get(&token).ok_or("this download was already answered")?;
    Ok(page.player.as_ref().and_then(|player| manager::video::from_player(player).ok()))
}

/// What the tab of Download Video `token` played, sorted out: plain files, playlists
/// listed from the text the page read, and playlists to ask yt-dlp about (`video_choices`).
#[tauri::command]
pub fn video_sources(state: State<AppState>, token: u64) -> Answer<Vec<Source>> {
    let videos = state.videos.lock().unwrap();
    let page = videos.get(&token).ok_or("this download was already answered")?;
    Ok(caught::sources(&page.caught))
}

/// The qualities of a video page, or of a playlist the browser caught (asked for with the
/// browser's `headers`).
#[tauri::command]
pub async fn video_choices(state: State<'_, AppState>, page: String, headers: Vec<(String, String)>) -> Answer<VideoInfo> {
    state.manager.video_choices(&page, &headers).await.map_err(text)
}

#[tauri::command]
pub fn list(state: State<AppState>) -> Vec<Download> {
    state.manager.list()
}

#[tauri::command]
pub fn get_download(state: State<AppState>, id: i64) -> Answer<Download> {
    state.manager.get(id).map_err(text)
}

/// The Download complete window's Check: a hash from the site compared with the file, or
/// the file's SHA-256.
#[tauri::command]
pub async fn check_hash(state: State<'_, AppState>, id: i64, expected: String) -> Answer<HashCheck> {
    state.manager.check_hash(id, &expected).await.map_err(text)
}

/// Options' Tools tab: what is installed.
#[tauri::command]
pub fn tools(state: State<AppState>) -> Vec<ToolInfo> {
    state.manager.tools()
}

/// Options' Update now: the newest release of each program.
#[tauri::command]
pub async fn update_tools(state: State<'_, AppState>) -> Answer<Vec<ToolUpdate>> {
    Ok(state.manager.update_tools().await)
}

#[tauri::command]
pub fn settings(state: State<AppState>) -> Settings {
    state.manager.settings()
}

#[tauri::command]
pub fn update_settings(state: State<AppState>, settings: Settings) -> Answer<()> {
    let limited = settings.speed_limit > 0;
    state.manager.update_settings(settings).map_err(text)?;
    state.show_limiter(limited);
    Ok(())
}

#[tauri::command]
pub fn set_category_dir(state: State<AppState>, name: String, dir: PathBuf) -> Answer<Category> {
    state.manager.set_category_dir(&name, dir).map_err(text)
}

#[tauri::command]
pub fn new_queue(state: State<AppState>, name: String) -> Answer<Queue> {
    state.manager.new_queue(&name).map_err(text)
}

#[tauri::command]
pub fn update_queue(state: State<AppState>, queue: Queue) -> Answer<Queue> {
    state.manager.update_queue(queue).map_err(text)
}

#[tauri::command]
pub async fn open_options(app: AppHandle) {
    windows::open_options(&app);
}

#[tauri::command]
pub fn pending(state: State<AppState>, token: u64) -> Answer<NewDownload> {
    state.pending.lock().unwrap().get(&token).cloned().ok_or_else(|| "this download was already answered".into())
}

#[derive(Serialize)]
pub struct UrlInfo {
    file_name: String,
    size: Option<u64>,
    ranges: bool,
}

/// What the server says about a link, for the File Info window.
#[tauri::command]
pub async fn check_url(url: String, headers: Vec<(String, String)>) -> Answer<UrlInfo> {
    let probe = engine::probe(&url, &headers).await.map_err(text)?;
    Ok(UrlInfo { file_name: probe.file_name, size: probe.size, ranges: probe.ranges })
}

#[tauri::command]
pub fn category_for(state: State<AppState>, file_name: String) -> Option<Category> {
    state.manager.category_for(&file_name)
}

#[tauri::command]
pub fn categories(state: State<AppState>) -> Vec<Category> {
    state.manager.categories()
}

#[tauri::command]
pub fn queues(state: State<AppState>) -> Vec<Queue> {
    state.manager.queues()
}

/// A line picked in Download Video.
#[derive(Deserialize)]
pub struct Pick {
    /// The format in the quality list it was picked from; none for a plain file the tab
    /// played.
    format: Option<String>,
    /// What the tab played (`Source::index`) when it is not the page itself.
    caught: Option<usize>,
}

/// The answer from Download File Info or Download Video: start now, or wait in `queue`.
/// A refused answer (a path that is no full path, say) leaves the window able to answer again.
#[tauri::command]
pub async fn confirm(
    state: State<'_, AppState>,
    token: u64,
    start: bool,
    path: PathBuf,
    description: String,
    queue: Option<i64>,
    pick: Option<Pick>,
) -> Answer<i64> {
    let file_name = path.file_name().and_then(|n| n.to_str()).ok_or("the Save As path needs a file name")?.to_owned();
    let dir = path.parent().filter(|d| d.is_absolute()).ok_or("the Save As path must be a full path")?.to_owned();
    let description = Some(description).filter(|d| !d.trim().is_empty());
    // Started while the window was open: it only needs the answer.
    let early = state.early.lock().unwrap().get(&token).copied();
    if let Some(id) = early {
        state.manager.confirm_provisional(id, dir, file_name, description, start, queue).await.map_err(text)?;
        state.early.lock().unwrap().remove(&token);
        state.pending.lock().unwrap().remove(&token);
        return Ok(id);
    }
    let (video, source) = pick.map_or((None, None), |p| (p.format, p.caught));
    // What the tab played, looked up before anything is taken out: a pick that names
    // nothing is refused with the window still able to answer.
    let played = match source {
        Some(index) => {
            let videos = state.videos.lock().unwrap();
            let page = videos.get(&token).ok_or("this download was already answered")?;
            let played = page.caught.get(index).ok_or("that stream is not in the list")?;
            // A playlist the page built has no address of its own: filed under the page's.
            let web = played.url.starts_with("http://") || played.url.starts_with("https://");
            let url = if web { played.url.clone() } else { page.url.clone() };
            Some((url, played.headers.clone(), caught::saved(&page.caught, index)))
        }
        None => None,
    };
    state.duplicates.lock().unwrap().remove(&token);
    let new = state.pending.lock().unwrap().remove(&token).ok_or("this download was already answered")?;
    state.videos.lock().unwrap().remove(&token);
    let new = match played {
        Some((url, headers, playlists)) => NewDownload { url, headers, playlists, ..new },
        None => new,
    };
    let added = state.manager.add(NewDownload {
        file_name: Some(file_name),
        dir: Some(dir),
        description,
        queue,
        start,
        video: video.or(new.video),
        ask: false,
        ..new
    });
    added.map(|d| d.id).map_err(text)
}

/// The window closed without an answer: the early download goes, with its files.
#[tauri::command]
pub async fn cancel_pending(state: State<'_, AppState>, token: u64) -> Answer<()> {
    state.pending.lock().unwrap().remove(&token);
    state.duplicates.lock().unwrap().remove(&token);
    // (With the playlists' texts its tab played.)
    state.videos.lock().unwrap().remove(&token);
    let early = state.early.lock().unwrap().remove(&token);
    if let Some(id) = early {
        state.manager.remove(id, true).await.map_err(text)?;
    }
    Ok(())
}

// Commands that open windows are async: a synchronous command runs on the window thread,
// and a window created there on Windows gets its frame but never loads its page.

/// Add URL in the main window: the same File Info window as a browser download, or Video
/// Info when the address is a web page (where yt-dlp looks for a video).
#[tauri::command]
pub async fn add_url(app: AppHandle, url: String) -> Answer<()> {
    let url = url.trim().to_owned();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("enter an http:// or https:// address".into());
    }
    let page = engine::probe(&url, &[]).await.is_ok_and(|p| p.is_web_page());
    if page {
        ask_video(&app, VideoPage { url, ..VideoPage::default() })
    } else {
        ask(&app, NewDownload { url, start: true, ..NewDownload::default() })
    }
    Ok(())
}

#[tauri::command]
pub async fn pause(state: State<'_, AppState>, id: i64) -> Answer<()> {
    state.manager.pause(id).await.map_err(text)
}

#[tauri::command]
pub fn resume(state: State<AppState>, id: i64) -> Answer<()> {
    state.manager.resume(id).map_err(text)
}

#[tauri::command]
pub async fn remove(state: State<'_, AppState>, id: i64, delete_file: bool) -> Answer<()> {
    state.manager.remove(id, delete_file).await.map_err(text)
}

#[tauri::command]
pub async fn stop_all(state: State<'_, AppState>) -> Answer<()> {
    for d in state.manager.list() {
        if d.status == manager::model::Status::Downloading {
            state.manager.pause(d.id).await.map_err(text)?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn start_queue(state: State<AppState>, id: i64) -> Answer<()> {
    state.manager.start_queue(id).map_err(text)
}

#[tauri::command]
pub async fn stop_queue(state: State<'_, AppState>, id: i64) -> Answer<()> {
    state.manager.stop_queue(id).await.map_err(text)
}

fn finished_path(state: &AppState, id: i64) -> Answer<PathBuf> {
    state.manager.get(id).map_err(text)?.path.ok_or_else(|| "the file is not complete yet".into())
}

#[tauri::command]
pub fn open_file(app: AppHandle, state: State<AppState>, id: i64) -> Answer<()> {
    let path = finished_path(&state, id)?;
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(text)
}

#[tauri::command]
pub fn open_folder(app: AppHandle, state: State<AppState>, id: i64) -> Answer<()> {
    let d = state.manager.get(id).map_err(text)?;
    match d.path {
        Some(path) => app.opener().reveal_item_in_dir(path).map_err(text),
        None => {
            let dir = d.dir.ok_or("the folder is not decided yet")?;
            app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(text)
        }
    }
}

/// A folder picker starting at `start`; None when cancelled.
#[tauri::command]
pub async fn browse_folder(app: AppHandle, start: Option<PathBuf>) -> Option<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file();
    if let Some(start) = start.filter(|s| s.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_folder(move |folder| {
        let _ = tx.send(folder.and_then(|f| f.into_path().ok()));
    });
    rx.await.ok().flatten()
}

/// Double-click on a running download in the list.
#[tauri::command]
pub async fn show_progress(app: AppHandle, id: i64) {
    windows::open_progress(&app, id);
}
