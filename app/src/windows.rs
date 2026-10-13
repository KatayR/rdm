//! IDM's windows: main list, Download File Info, progress, and complete.

use std::sync::Mutex;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};

pub const MAIN: &str = "main";

pub fn progress_label(id: i64) -> String {
    format!("progress-{id}")
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    open(app, MAIN, "main.html", "RDM", (960.0, 560.0), true, false);
}

/// Download File Info for a pending download.
pub fn open_info(app: &AppHandle, token: u64) {
    open_fitted(app, &format!("info-{token}"), &format!("info.html?token={token}"), "Download File Info", 580.0, true);
}

/// Video Info: the qualities of a video page, for a pending download.
pub fn open_video(app: &AppHandle, token: u64) {
    open(app, &format!("video-{token}"), &format!("video.html?token={token}"), "Download Video", (620.0, 600.0), true, true);
}

pub fn open_progress(app: &AppHandle, id: i64) {
    let label = progress_label(id);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return;
    }
    open_fitted(app, &label, &format!("progress.html?id={id}"), "Download", 500.0, false);
}

/// A download's Properties; one window per download.
pub fn open_properties(app: &AppHandle, id: i64) {
    let label = format!("properties-{id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return;
    }
    open_fitted(app, &label, &format!("properties.html?id={id}"), "Properties", 560.0, false);
}

/// The question before the downloads `ids` are removed. One at a time: asked again for the
/// same downloads, the open one comes to the front; asked for others, it gives way, so an
/// answer never removes an earlier selection. (A closing window keeps its label for a
/// moment, hence a new one each time.)
pub fn open_delete(app: &AppHandle, ids: &[i64]) {
    static ASKED: Mutex<(u64, Vec<i64>)> = Mutex::new((0, Vec::new()));
    let mut asked = ASKED.lock().unwrap();
    if let Some(w) = app.get_webview_window(&format!("delete-{}", asked.0)) {
        if asked.1 == ids {
            let _ = w.set_focus();
            return;
        }
        let _ = w.close();
    }
    *asked = (asked.0 + 1, ids.to_vec());
    let ids = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    open_fitted(app, &format!("delete-{}", asked.0), &format!("delete.html?ids={ids}"), "Delete", 460.0, true);
}

pub fn open_complete(app: &AppHandle, id: i64) {
    open_fitted(app, &format!("complete-{id}"), &format!("complete.html?id={id}"), "Download complete", 520.0, true);
}

pub fn open_options(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("options") {
        let _ = w.set_focus();
        return;
    }
    open(app, "options", "options.html", "RDM Options", (600.0, 470.0), false, false);
}

/// The question when a newer RDM was found. One at a time.
pub fn open_update(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("update") {
        let _ = w.set_focus();
        return;
    }
    open_fitted(app, "update", "update.html", "RDM update", 460.0, true);
}

pub fn close(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.close();
    }
}

fn open(app: &AppHandle, label: &str, page: &str, title: &str, size: (f64, f64), resizable: bool, on_top: bool) {
    let built = builder(app, label, page, title, size, resizable, on_top).build();
    if let Err(e) = built {
        eprintln!("rdm-app: could not open {label}: {e}");
    }
}

/// A window that takes its content's height: hidden until the page has fitted it
/// (common.js showWhenReady).
fn open_fitted(app: &AppHandle, label: &str, page: &str, title: &str, width: f64, on_top: bool) {
    let built = builder(app, label, page, title, (width, 300.0), false, on_top).visible(false).build();
    if let Err(e) = built {
        eprintln!("rdm-app: could not open {label}: {e}");
    }
}

fn builder<'a>(
    app: &'a AppHandle,
    label: &str,
    page: &str,
    title: &str,
    size: (f64, f64),
    resizable: bool,
    on_top: bool,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    let builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title(title)
        .inner_size(size.0, size.1)
        .resizable(resizable)
        .always_on_top(on_top)
        .center()
        .focused(true);
    // Only a window the person can resize needs a floor; fixed ones may fit their content.
    if resizable { builder.min_inner_size(size.0.min(480.0), size.1.min(240.0)) } else { builder }
}
