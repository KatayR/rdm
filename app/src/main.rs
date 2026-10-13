//! `rdm-app`: the manager with IDM's windows and a tray icon. It serves the same socket
//! as `rdm daemon`, so the command line and the browser helper work with either.
//! Started with `--background` (by the helper or the CLI) it opens only the tray.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod update;
mod windows;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use engine::CancellationToken;
use manager::ipc::{Client, Request, UiRequest, is_other_version, listen, serve};
use manager::model::{Event, NewDownload, VideoPage};
use manager::{Config, Manager};
use tauri::async_runtime::{block_on, spawn};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager as _, RunEvent, WindowEvent};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::UnboundedReceiver;

pub struct AppState {
    pub manager: Manager,
    /// Downloads waiting for an answer in a Download File Info window, by token.
    pub pending: Mutex<HashMap<u64, NewDownload>>,
    /// For a pending video: the page and what its tab played, by the same token.
    pub videos: Mutex<HashMap<u64, VideoPage>>,
    /// For a pending file: the download that started while its window is open, by token.
    pub early: Mutex<HashMap<u64, i64>>,
    /// For a pending file whose address is already in the list: that download, by token.
    pub duplicates: Mutex<HashMap<u64, i64>>,
    next_token: AtomicU64,
    quitting: AtomicBool,
    /// Ends the serving of the socket.
    stop: CancellationToken,
    /// RDM's own update: what was found, and how far its download is.
    pub updates: update::Updates,
    /// The tray's Speed limiter item, kept in step when Options changes the limit.
    limiter_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

impl AppState {
    /// RDM is closing, or closed down for its update.
    pub fn closed_down(&self) -> bool {
        self.quitting.load(Ordering::Relaxed)
    }

    pub fn show_limiter(&self, on: bool) {
        if let Some(item) = self.limiter_item.lock().unwrap().as_ref() {
            let _ = item.set_checked(on);
        }
    }

    pub fn add_pending(&self, new: NewDownload) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(token, new);
        token
    }
}

fn main() {
    let home = manager::home();
    let background = std::env::args().any(|a| a == "--background");
    log_panics(home.join("app.log"));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::list,
            commands::get_download,
            commands::tools,
            commands::update_tools,
            commands::early_download,
            commands::duplicate_of,
            commands::continue_duplicate,
            commands::same_file,
            commands::refresh_address,
            commands::show_download,
            commands::open_properties,
            commands::open_delete,
            commands::set_description,
            commands::check_hash,
            commands::settings,
            commands::update_settings,
            commands::pending,
            commands::video_page,
            commands::video_sources,
            commands::video_ahead,
            commands::check_url,
            commands::category_for,
            commands::categories,
            commands::queues,
            commands::confirm,
            commands::cancel_pending,
            commands::add_url,
            commands::pause,
            commands::resume,
            commands::remove,
            commands::stop_all,
            commands::start_queue,
            commands::stop_queue,
            commands::open_file,
            commands::open_folder,
            commands::browse_folder,
            commands::show_progress,
            commands::set_category_dir,
            commands::new_queue,
            commands::update_queue,
            commands::open_options,
            commands::video_choices,
            update::app_version,
            update::update_check,
            update::open_update,
            update::update_found,
            update::update_progress,
            update::update_install,
        ])
        .setup(move |app| {
            // One app per data folder: a second start brings the first one forward.
            let listener = match block_on(listen(&home)) {
                Ok(listener) => listener,
                Err(taken) => {
                    match block_on(Client::connect(&home)) {
                        Ok(mut client) => drop(block_on(client.call(&Request::Show))),
                        // The RDM that runs is another version than this program.
                        Err(e) if is_other_version(&e) => fatal(&e.to_string()),
                        Err(_) => fatal(&format!("RDM could not start.\n\n{taken}")),
                    }
                    std::process::exit(0);
                }
            };
            let manager = match block_on(async { Manager::open(Config::new(home.clone())) }) {
                Ok(manager) => manager,
                Err(e) => {
                    // Not while the message is up: a caller would wait at an open pipe
                    // that nobody answers.
                    drop(listener);
                    fatal(&format!("RDM could not start.\n\n{e}"))
                }
            };
            let stop = CancellationToken::new();
            app.manage(AppState {
                manager: manager.clone(),
                stop: stop.clone(),
                updates: update::Updates::default(),
                pending: Mutex::new(HashMap::new()),
                videos: Mutex::new(HashMap::new()),
                early: Mutex::new(HashMap::new()),
                duplicates: Mutex::new(HashMap::new()),
                next_token: AtomicU64::new(1),
                quitting: AtomicBool::new(false),
                limiter_item: Mutex::new(None),
            });

            let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
            {
                let (manager, stop) = (manager.clone(), stop.clone());
                spawn(async move { serve(&manager, listener, stop, Some(ui_tx)).await });
            }
            spawn(ui_requests(app.handle().clone(), ui_rx));
            spawn(follow_downloads(app.handle().clone(), manager.subscribe()));
            {
                // A Shutdown request on the socket (`rdm shutdown`) quits the app.
                let handle = app.handle().clone();
                spawn(async move {
                    stop.cancelled().await;
                    quit(&handle).await;
                });
            }
            tray(app.handle()).unwrap_or_else(|e| fatal(&format!("RDM could not start: no tray icon.\n\n{e}")));
            if !background {
                windows::show_main(app.handle());
            }
            spawn(update::keep_looking(app.handle().clone()));
            #[cfg(target_os = "macos")]
            std::thread::spawn(tell_browsers);
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || update::clear_old_setups(&handle));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window hides it to the tray, as IDM does.
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == windows::MAIN
                && !window.state::<AppState>().quitting.load(Ordering::Relaxed)
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| fatal(&format!("RDM could not start.\n\n{e}")))
        .run(|app, event| match event {
            // Closing the last window keeps the tray; only Exit (or Shutdown) quits.
            RunEvent::ExitRequested { api, code: None, .. } if !app.state::<AppState>().quitting.load(Ordering::Relaxed) => {
                api.prevent_exit();
            }
            // A click on the Dock icon: the main window, which closing only hid.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => windows::show_main(app),
            // An end that did not come through `quit`: on a Mac, Quit in the menu bar or the
            // Dock (Cmd+Q), and the system shutting down. The downloads are stopped as Exit
            // stops them.
            RunEvent::Exit => {
                let state = app.state::<AppState>();
                if !state.quitting.swap(true, Ordering::Relaxed) {
                    block_on(state.manager.shutdown());
                }
            }
            _ => {}
        });
}

/// Shows why the app cannot start, and ends it: there is no console, and no window of the
/// app's own yet.
pub(crate) fn fatal(text: &str) -> ! {
    eprintln!("rdm-app: {text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide("RDM"));
        // SAFETY: both strings end in a NUL and live until the call returns.
        unsafe {
            // On top: started in the background (by the browser's helper) it has no window
            // to be in front of.
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST);
        }
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_core_foundation::{CFString, CFUserNotification, kCFUserNotificationStopAlertLevel};
        let (text, title) = (CFString::from_str(text), CFString::from_static_str("RDM"));
        let mut pressed = 0;
        // SAFETY: both strings live until the call returns, and `pressed` is a place for
        // the button it reports.
        unsafe {
            // The system's own alert, which is above every window and needs none of the
            // app's. No time limit: it stays until it is answered.
            let level = kCFUserNotificationStopAlertLevel;
            CFUserNotification::display_alert(0.0, level, None, None, None, Some(&title), Some(&text), None, None, None, &mut pressed);
        }
    }
    std::process::exit(1)
}

/// What the setup does on Windows once the programs are in place, the app does on a Mac at
/// every start, as there is no setup: `rdm browser install` tells the browsers where this
/// copy's helper is and writes the extension's files from it. So that also holds after an
/// update, and after the app was moved. Not for a copy run from the build folder.
#[cfg(target_os = "macos")]
fn tell_browsers() {
    use std::process::{Command, Stdio};
    if !update::installed() {
        return;
    }
    let Ok(me) = std::env::current_exe() else { return };
    let mut install = Command::new(me.with_file_name("rdm"));
    let told = install.args(["browser", "install"]).stdin(Stdio::null()).stdout(Stdio::null()).output();
    match told {
        Ok(output) if output.status.success() => {}
        Ok(output) => eprintln!("rdm-app: rdm browser install: {}", String::from_utf8_lossy(&output.stderr).trim()),
        Err(e) => eprintln!("rdm-app: rdm browser install: {e}"),
    }
}

/// The app has no console, so a panic would vanish (a panic on the window thread even
/// aborts the process): append each one to app.log in the data folder first.
fn log_panics(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let backtrace = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{secs} panic: {info}\n{backtrace}");
        }
        default(info);
    }));
}

/// Requests from the socket that need a window.
async fn ui_requests(app: AppHandle, mut rx: UnboundedReceiver<UiRequest>) {
    while let Some(request) = rx.recv().await {
        match request {
            UiRequest::Ask(new) => commands::ask(&app, new),
            UiRequest::ChooseVideo(page) => commands::ask_video(&app, page),
            UiRequest::ShowMain => windows::show_main(&app),
        }
    }
}

/// Opens progress and complete windows as downloads start and finish.
async fn follow_downloads(app: AppHandle, mut events: tokio::sync::broadcast::Receiver<Event>) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let settings = app.state::<AppState>().manager.settings();
        match event {
            Event::Started(id) if settings.show_progress => windows::open_progress(&app, id),
            Event::Complete(id) => {
                windows::close(&app, &windows::progress_label(id));
                if settings.show_complete {
                    windows::open_complete(&app, id);
                }
            }
            // The progress window shows why it failed; open one if it was not shown.
            Event::Failed(id) => windows::open_progress(&app, id),
            _ => {}
        }
    }
}

fn tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open RDM", true, None::<&str>)?;
    let options = MenuItem::with_id(app, "options", "Options", true, None::<&str>)?;
    let limited = app.state::<AppState>().manager.settings().speed_limit > 0;
    let limiter = CheckMenuItem::with_id(app, "limiter", "Speed limiter", true, limited, None::<&str>)?;
    let start = MenuItem::with_id(app, "start-queue", "Start main queue", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop-queue", "Stop main queue", true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "exit", "Exit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&open, &options, &separator, &limiter, &separator, &start, &stop, &separator, &exit],
    )?;
    *app.state::<AppState>().limiter_item.lock().unwrap() = Some(limiter);
    // On a Mac a click on a menu bar icon opens its menu, whose first line opens RDM.
    let mut builder =
        TrayIconBuilder::with_id("main").tooltip("RDM").menu(&menu).show_menu_on_left_click(cfg!(target_os = "macos"));
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder
        .on_menu_event(|app, event| {
            let manager = app.state::<AppState>().manager.clone();
            match event.id().as_ref() {
                "open" => windows::show_main(app),
                "options" => windows::open_options(app),
                "limiter" => toggle_limiter(app),
                "start-queue" => {
                    let _ = manager.start_queue(manager::MAIN_QUEUE);
                }
                "stop-queue" => {
                    spawn(async move {
                        let _ = manager.stop_queue(manager::MAIN_QUEUE).await;
                    });
                }
                "exit" => {
                    let app = app.clone();
                    spawn(async move { quit(&app).await });
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if cfg!(not(target_os = "macos"))
                && let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
            {
                windows::show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// IDM's tray toggle: off remembers the limit, on restores it. With no limit set yet,
/// Options opens to set one.
fn toggle_limiter(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut settings = state.manager.settings();
    if settings.speed_limit > 0 {
        settings.speed_limit_value = settings.speed_limit;
        settings.speed_limit = 0;
    } else if settings.speed_limit_value > 0 {
        settings.speed_limit = settings.speed_limit_value;
    } else {
        state.show_limiter(false);
        windows::open_options(app);
        return;
    }
    let on = settings.speed_limit > 0;
    if let Err(e) = state.manager.update_settings(settings) {
        eprintln!("rdm-app: {e}");
    }
    state.show_limiter(on);
}

/// Closes RDM down for its update, as `quit` does, but stays: the socket goes first, so
/// nothing new is taken on, then the downloads stop (and with them the ffmpeg of a join).
/// The update's setup must find none of RDM's programs at work.
pub(crate) async fn close_down(app: &AppHandle) {
    let state = app.state::<AppState>();
    // Before the socket's end is seen by what quits the app on a Shutdown request.
    state.quitting.store(true, Ordering::Relaxed);
    state.stop.cancel();
    state.manager.shutdown().await;
}

/// Stops the downloads (they resume on the next start) and exits.
async fn quit(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.quitting.swap(true, Ordering::Relaxed) {
        return;
    }
    state.manager.shutdown().await;
    app.exit(0);
}
