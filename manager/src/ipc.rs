//! The daemon's local socket: a named pipe on Windows, a Unix socket in the data folder on
//! macOS. One JSON request per line, one JSON response per line. Web pages cannot reach a
//! local socket, unlike a localhost port.
//!
//! A connection's first line is the caller's version (`Hello`). The programs are installed
//! together, so another version means an update that did not replace every program (one
//! was running), or a copy from another folder: it is refused in words, not served with
//! requests that may mean something else to it.

use std::io;
use std::path::Path;
use std::time::Duration;

use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{ListenerOptions, Name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use engine::CancellationToken;

use crate::model::{NewDownload, Queue, VideoPage};
use crate::{Config, Manager};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Add(NewDownload),
    List,
    Pause { ids: Vec<i64> },
    Resume { ids: Vec<i64> },
    Remove { ids: Vec<i64>, delete_file: bool },
    Move { ids: Vec<i64>, queue: i64 },
    /// Bytes per second for all downloads; 0 lifts the limit.
    SpeedLimit { rate: u64 },
    DownloadLimit { id: i64, rate: u64 },
    Connections { connections: u16 },
    Settings,
    UpdateSettings(crate::model::Settings),
    Categories,
    CategoryDir { name: String, dir: std::path::PathBuf },
    Queues,
    NewQueue { name: String },
    UpdateQueue(Queue),
    StartQueue { id: i64 },
    StopQueue { id: i64 },
    /// Bring the main window forward (a second start of the app sends this).
    Show,
    /// A video page (the browser's "Download this video") with what its tab played: the
    /// person picks a quality in the Download Video window. Needs the app.
    ChooseVideo(VideoPage),
    /// A video page whose video the browser saw play: listed in the background, so its
    /// quality list is ready when the button is pressed.
    ListAhead { page: String, headers: Vec<(String, String)> },
    Shutdown,
}

/// What the socket hands to an interface when one runs (the app, not the bare daemon).
#[derive(Debug)]
pub enum UiRequest {
    /// A download to confirm in the Download File Info window before it is added.
    Ask(NewDownload),
    /// A video page to choose a quality for.
    ChooseVideo(VideoPage),
    ShowMain,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(Value),
    Error(String),
}

/// The version every program of one installation has.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A connection's first line.
#[derive(Serialize, Deserialize)]
struct Hello {
    hello: String,
}

fn other_version(caller: &str) -> String {
    format!(
        "RDM was updated only in part: the running RDM is version {VERSION}, the program calling it is {caller}. Close RDM (its tray icon, Exit) and run its setup again."
    )
}

/// Whether `Client::connect` failed because the manager is another version (and not
/// because nobody answered).
pub fn is_other_version(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
}

/// FNV-1a, written out: every program must make the same name of the same folder, whatever
/// compiler built it, and the standard library's hasher promises no such thing.
fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// A data folder's number in its pipe's name. Made of the path's parts, so `D:\rdm`,
/// `D:/rdm` and `D:\rdm\` are one folder with one manager, as they are on disk.
fn folder_number(home: &Path) -> u64 {
    // 0xff ends each part: it is in no UTF-8 text, so "ab" + "c" is not "a" + "bc".
    fnv1a(home.components().flat_map(|part| part.as_os_str().as_encoded_bytes().iter().copied().chain([0xff])))
}

/// Socket name for a data folder, so test instances never meet the real daemon.
pub fn socket_name(home: &Path) -> io::Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        Ok(format!("rdm-{:016x}", folder_number(home)).to_ns_name::<GenericNamespaced>()?.into_owned())
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        Ok(home.join("daemon.sock").to_fs_name::<GenericFilePath>()?.into_owned())
    }
}

/// Connecting to a named pipe whose instances are all busy waits indefinitely; a live
/// daemon always has a free one, so a slow connect means nobody is serving.
async fn connect(home: &Path) -> io::Result<Stream> {
    let name = socket_name(home)?;
    tokio::time::timeout(Duration::from_secs(3), Stream::connect(name))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the daemon did not answer"))?
}

/// Runs the manager without an interface and serves its socket until a Shutdown request
/// (or `stop`), then stops the downloads so the next start resumes them.
pub async fn run_daemon(config: Config, stop: CancellationToken) -> crate::Result<()> {
    let listener = listen(&config.home).await?;
    let manager = Manager::open(config)?;
    serve(&manager, listener, stop, None).await;
    manager.shutdown().await;
    Ok(())
}

/// Takes the socket for `home`; fails when a daemon or the app already serves it.
pub async fn listen(home: &Path) -> crate::Result<Listener> {
    if connect(home).await.is_ok() {
        return Err(crate::Error::Invalid("the daemon is already running".into()));
    }
    // Nobody answered, so a leftover socket file (macOS, after a crash) can be replaced.
    Ok(ListenerOptions::new().name(socket_name(home)?).try_overwrite(true).create_tokio()?)
}

/// Answers requests until a Shutdown request or `stop`. With `ui`, downloads that ask for
/// confirmation and Show requests go to the interface.
pub async fn serve(manager: &Manager, listener: Listener, stop: CancellationToken, ui: Option<UnboundedSender<UiRequest>>) {
    loop {
        let conn = tokio::select! {
            conn = listener.accept() => conn,
            _ = stop.cancelled() => return,
        };
        match conn {
            Ok(conn) => {
                tokio::spawn(handle_connection(manager.clone(), conn, stop.clone(), ui.clone()));
            }
            Err(e) => eprintln!("rdm: connection failed: {e}"),
        }
    }
}

async fn handle_connection(
    manager: Manager,
    conn: Stream,
    stop: CancellationToken,
    ui: Option<UnboundedSender<UiRequest>>,
) {
    let mut lines = BufReader::new(&conn).lines();
    let mut writer = &conn;
    let mut greeted = false;
    loop {
        // Connections close when the daemon stops, so its socket goes away with it.
        let line = tokio::select! {
            line = lines.next_line() => line,
            _ = stop.cancelled() => return,
        };
        let Ok(Some(line)) = line else { return };
        let response = match serde_json::from_str::<Request>(&line) {
            // The first line says who is calling; nothing is done for another version.
            _ if !greeted => match serde_json::from_str::<Hello>(&line) {
                Ok(Hello { hello }) if hello == VERSION => Response::Ok(Value::Null),
                Ok(Hello { hello }) => Response::Error(other_version(&format!("version {hello}"))),
                Err(_) => Response::Error(other_version("an earlier version")),
            },
            Ok(Request::Shutdown) => {
                stop.cancel();
                Response::Ok(Value::Null)
            }
            Ok(request) => match handle(&manager, request, ui.as_ref()).await {
                Ok(value) => Response::Ok(value),
                Err(e) => Response::Error(e.to_string()),
            },
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let refused = !greeted && matches!(response, Response::Error(_));
        greeted = true;
        let mut out = serde_json::to_vec(&response).expect("responses serialize");
        out.push(b'\n');
        if writer.write_all(&out).await.is_err() || refused {
            return;
        }
    }
}

async fn handle(m: &Manager, request: Request, ui: Option<&UnboundedSender<UiRequest>>) -> crate::Result<Value> {
    Ok(match request {
        Request::Add(new) => match ui {
            Some(ui) if new.ask => {
                ui.send(UiRequest::Ask(new)).map_err(|_| crate::Error::Invalid("the interface is closing".into()))?;
                json!({ "asked": true })
            }
            _ => json!(m.add(new)?),
        },
        Request::Show => {
            let ui = ui.ok_or_else(|| crate::Error::Invalid("the manager runs without windows".into()))?;
            ui.send(UiRequest::ShowMain).map_err(|_| crate::Error::Invalid("the interface is closing".into()))?;
            Value::Null
        }
        Request::ChooseVideo(page) => {
            let ui = ui.ok_or_else(|| crate::Error::Invalid("choosing a video quality needs the RDM app".into()))?;
            ui.send(UiRequest::ChooseVideo(page)).map_err(|_| crate::Error::Invalid("the interface is closing".into()))?;
            json!({ "asked": true })
        }
        Request::ListAhead { page, headers } => {
            m.list_ahead(page, headers);
            Value::Null
        }
        Request::List => json!(m.list()),
        Request::Pause { ids } => {
            for id in ids {
                m.pause(id).await?;
            }
            Value::Null
        }
        Request::Resume { ids } => {
            for id in ids {
                m.resume(id)?;
            }
            Value::Null
        }
        Request::Remove { ids, delete_file } => {
            for id in ids {
                m.remove(id, delete_file).await?;
            }
            Value::Null
        }
        Request::Move { ids, queue } => json!(m.move_to_queue(&ids, queue)?),
        Request::SpeedLimit { rate } => json!(m.set_speed_limit(rate)?),
        Request::DownloadLimit { id, rate } => json!(m.set_download_limit(id, rate)?),
        Request::Connections { connections } => json!(m.set_connections(connections)?),
        Request::Settings => json!(m.settings()),
        Request::UpdateSettings(settings) => json!(m.update_settings(settings)?),
        Request::Categories => json!(m.categories()),
        Request::CategoryDir { name, dir } => json!(m.set_category_dir(&name, dir)?),
        Request::Queues => json!(m.queues()),
        Request::NewQueue { name } => json!(m.new_queue(&name)?),
        Request::UpdateQueue(q) => json!(m.update_queue(q)?),
        Request::StartQueue { id } => json!(m.start_queue(id)?),
        Request::StopQueue { id } => json!(m.stop_queue(id).await?),
        Request::Shutdown => unreachable!("handled by the connection"),
    })
}

/// A connection to the daemon.
pub struct Client {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<Stream>>>,
    writer: tokio::io::WriteHalf<Stream>,
}

impl Client {
    /// Connects and says which version is calling. A manager of another version refuses:
    /// that error holds its words and is the one `is_other_version` knows.
    pub async fn connect(home: &Path) -> io::Result<Client> {
        Client::connect_as(home, VERSION).await
    }

    /// `connect`, calling as `version` (tests call as another).
    #[doc(hidden)]
    pub async fn connect_as(home: &Path, version: &str) -> io::Result<Client> {
        let stream = connect(home).await?;
        let (read, writer) = tokio::io::split(stream);
        let mut client = Client { lines: BufReader::new(read).lines(), writer };
        // A pipe can be open with nobody reading it yet (a manager still starting, or
        // stuck): no answer to the first line is no manager.
        let hello = Hello { hello: version.into() };
        let greeted = tokio::time::timeout(Duration::from_secs(10), client.exchange(&hello)).await;
        match greeted.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the manager did not answer"))?? {
            Ok(_) => Ok(client),
            Err(refusal) => Err(io::Error::new(io::ErrorKind::Unsupported, refusal)),
        }
    }

    /// Sends one request; Err holds the daemon's error message.
    pub async fn call(&mut self, request: &Request) -> io::Result<Result<Value, String>> {
        self.exchange(request).await
    }

    async fn exchange(&mut self, line: &impl Serialize) -> io::Result<Result<Value, String>> {
        let mut line = serde_json::to_vec(line)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        let reply = self.lines.next_line().await?.ok_or(io::ErrorKind::UnexpectedEof)?;
        Ok(match serde_json::from_str(&reply)? {
            Response::Ok(value) => Ok(value),
            Response::Error(e) => Err(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published values of FNV-1a (64 bit): the name of a folder's pipe never changes.
    #[test]
    fn pipe_names_come_from_a_fixed_hash() {
        assert_eq!(fnv1a(*b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(*b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(*b"foobar"), 0x8594_4171_f739_67e8);
        // A folder's number is that hash of its parts, each ended by 0xff.
        assert_eq!(folder_number(Path::new("data")), fnv1a(*b"data\xff"));
        assert_eq!(folder_number(Path::new("a/bc")), fnv1a(*b"a\xffbc\xff"));
        assert_ne!(folder_number(Path::new("ab/c")), folder_number(Path::new("a/bc")));
    }

    /// One folder, however it is written, has one pipe: a second manager on it would
    /// write the same database.
    #[cfg(windows)]
    #[test]
    fn spellings_of_one_folder_share_a_pipe() {
        let number = folder_number(Path::new(r"D:\rdm\home"));
        for spelling in [r"D:\rdm\home\", "D:/rdm/home", r"D:\rdm\.\home", r"D:\rdm//home"] {
            assert_eq!(folder_number(Path::new(spelling)), number, "{spelling}");
        }
        assert_ne!(folder_number(Path::new(r"D:\rdm\home2")), number);
    }
}
