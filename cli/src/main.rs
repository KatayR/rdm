use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use engine::{CancellationToken, Options};
use manager::ipc::Request;
use manager::model::{Category, Download, NewDownload, Queue, Settings};
use manager::schedule::parse_days;
use manager::tools::{Tool, Tools, Update};

use rdm::browser;
use rdm::fmt::{human, parse_rate};
use rdm::get;
use rdm::remote;

/// RDM, the Rusty Download Manager. Downloads run in a background manager that starts by itself;
/// `get` downloads one file in the foreground instead.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download one file now, in this window, without the manager
    Get {
        url: String,
        /// Connections to open
        #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=32))]
        connections: u16,
        /// Folder to save into
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        /// File name instead of the server's
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Add a download; it starts at once unless --later puts it in a queue
    Add {
        url: String,
        /// Wait in the queue instead of starting now
        #[arg(long)]
        later: bool,
        /// Queue id (default: the main queue)
        #[arg(long)]
        queue: Option<i64>,
        /// Folder (default: the category's folder)
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// File name instead of the server's
        #[arg(short, long)]
        name: Option<String>,
        /// Connections (default: the manager's setting)
        #[arg(short, long, value_parser = clap::value_parser!(u16).range(1..=32))]
        connections: Option<u16>,
        #[arg(long)]
        referer: Option<String>,
        #[arg(long)]
        cookie: Option<String>,
        /// Extra request header, "Name: value" (repeatable)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Speed limit for this download (500K, 2M)
        #[arg(long)]
        limit: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// The URL is a video page: download its video in this format (see `formats`)
        #[arg(long, value_name = "FORMAT")]
        video: Option<String>,
    },
    /// List the qualities a video page or an HLS / DASH playlist offers, with the format to
    /// pass to add --video
    Formats {
        url: String,
        #[arg(long)]
        referer: Option<String>,
        /// Extra request header, "Name: value" (repeatable)
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
    },
    /// Show all downloads
    #[command(alias = "ls")]
    List,
    /// Stop downloads; queues skip them until resumed
    Pause {
        #[arg(required = true)]
        ids: Vec<i64>,
    },
    /// Start downloads now, outside their queue's limit
    Resume {
        #[arg(required = true)]
        ids: Vec<i64>,
    },
    /// Remove downloads from the list and delete their partial files
    #[command(alias = "rm")]
    Remove {
        #[arg(required = true)]
        ids: Vec<i64>,
        /// Delete finished files too
        #[arg(long)]
        delete: bool,
    },
    /// Move downloads to the end of another queue
    Move {
        #[arg(required = true)]
        ids: Vec<i64>,
        #[arg(long)]
        queue: i64,
    },
    /// Show or set the speed limit for all downloads (500K, 2M, off), or one with --id
    Limit {
        rate: Option<String>,
        #[arg(long)]
        id: Option<i64>,
    },
    /// Default connections per download
    Connections {
        #[arg(value_parser = clap::value_parser!(u16).range(1..=32))]
        connections: u16,
    },
    /// Queues: list, new, start, stop, set
    Queue {
        #[command(subcommand)]
        command: Option<QueueCmd>,
    },
    /// Show categories, or set one's folder: categories Video D:\Videos
    Categories {
        #[arg(requires = "dir")]
        name: Option<String>,
        dir: Option<PathBuf>,
    },
    /// Connect Chrome and Brave: install or uninstall the extension files and host
    Browser {
        #[command(subcommand)]
        command: BrowserCmd,
    },
    /// Programs for video downloads (yt-dlp, Deno): show them, or update them
    Tools {
        #[command(subcommand)]
        command: Option<ToolsCmd>,
    },
    /// Run the manager in this window (normally it starts by itself)
    Daemon,
    /// Stop the manager; running downloads resume when it starts again
    Shutdown,
}

#[derive(Subcommand)]
enum BrowserCmd {
    /// Write the extension files and register the host for the current user
    Install,
    /// Remove the host registration
    Uninstall,
}

#[derive(Subcommand)]
enum ToolsCmd {
    List,
    /// Install or update to the newest official releases, checksums and signature checked;
    /// yt-dlp stays on its channel unless told otherwise
    Update {
        /// Switch yt-dlp to its nightly builds
        #[arg(long, conflicts_with = "stable")]
        nightly: bool,
        /// Switch yt-dlp back to its stable releases
        #[arg(long)]
        stable: bool,
    },
}

#[derive(Subcommand)]
enum QueueCmd {
    List,
    New {
        name: String,
    },
    Start {
        id: i64,
    },
    Stop {
        id: i64,
    },
    /// Change a queue; --start/--stop take HH:MM or off, --days e.g. mon,tue or all
    Set {
        id: i64,
        #[arg(long)]
        name: Option<String>,
        /// Files downloaded at the same time
        #[arg(long)]
        parallel: Option<u32>,
        /// Extra runs for a file that failed
        #[arg(long)]
        retries: Option<u32>,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        stop: Option<String>,
        #[arg(long)]
        days: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Cmd) -> Result<ExitCode, String> {
    let home = manager::home();
    let request = match command {
        Cmd::Get { url, connections, out, name } => {
            let opts = Options { connections: connections.into(), dir: out, file_name: name, ..Options::default() };
            return Ok(get::run(&url, opts).await);
        }
        Cmd::Daemon => {
            let stop = CancellationToken::new();
            let on_ctrl_c = stop.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    on_ctrl_c.cancel();
                }
            });
            manager::ipc::run_daemon(manager::Config::new(home), stop).await.map_err(|e| e.to_string())?;
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Browser { command } => {
            match command {
                BrowserCmd::Install => browser::install(&home)?,
                BrowserCmd::Uninstall => browser::uninstall(&home)?,
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Formats { url, referer, headers } => {
            let tools = Tools::new(home.join("tools")).map_err(|e| e.to_string())?;
            let headers = request_headers(referer, None, headers)?;
            let info = manager::video::choices(&tools, &manager::video::Listings::default(), &url, &headers).await.map_err(|e| e.to_string())?;
            println!("{}", info.title);
            for c in info.choices {
                let size = c.size.map(|s| format!("{}{}", if c.approx { "~" } else { "" }, human(s as f64))).unwrap_or_default();
                let stream = c.stream.unwrap_or_default();
                let ext = c.ext.to_ascii_uppercase();
                println!("  {:<20} {:<5} {:<4} {:<24} {:<14} {:>11}", c.format, ext, stream, c.label, c.codecs, size);
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Tools { command } => {
            let tools = Tools::new(home.join("tools")).map_err(|e| e.to_string())?;
            match command.unwrap_or(ToolsCmd::List) {
                ToolsCmd::List => {
                    for tool in Tool::ALL {
                        match tools.installed(tool) {
                            Some(i) => println!("{:<7} {}{}", tool.name(), i.version, if i.nightly { " (nightly)" } else { "" }),
                            None => println!("{:<7} not installed", tool.name()),
                        }
                    }
                }
                ToolsCmd::Update { nightly, stable } => {
                    for tool in Tool::ALL {
                        // Only yt-dlp has a nightly channel; it keeps the one installed, as
                        // Options' Update now does, unless one is asked for.
                        let installed = tools.installed(tool).is_some_and(|i| i.nightly);
                        let nightly = tool == Tool::YtDlp && (nightly || installed && !stable);
                        match tools.update(tool, nightly).await.map_err(|e| format!("{}: {e}", tool.name()))? {
                            Update::Installed(v) => println!("{:<7} {v} installed", tool.name()),
                            Update::UpToDate(v) => println!("{:<7} {v} is the newest", tool.name()),
                        }
                    }
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Shutdown => {
            let mut client = remote::connect(&home, false).await?;
            remote::call(&mut client, &Request::Shutdown).await?;
            println!("manager stopped");
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Add { url, later, queue, out, name, connections, referer, cookie, headers, limit, description, video } => {
            let all = request_headers(referer, cookie, headers)?;
            // The manager runs in another folder, so relative paths are resolved here.
            let dir = out.map(std::path::absolute).transpose().map_err(|e| e.to_string())?;
            let speed_limit = limit.as_deref().map(parse_rate).transpose()?.filter(|&r| r > 0);
            Request::Add(NewDownload {
                url,
                headers: all,
                file_name: name,
                dir,
                queue,
                start: !later,
                connections,
                speed_limit,
                description,
                video,
                ask: false,
                provisional: false,
                playlists: Vec::new(),
            })
        }
        Cmd::List => {
            let mut client = remote::connect(&home, true).await?;
            let list: Vec<Download> = remote::fetch(&mut client, &Request::List).await?;
            remote::print_downloads(&list);
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Pause { ids } => Request::Pause { ids },
        Cmd::Resume { ids } => Request::Resume { ids },
        Cmd::Remove { ids, delete } => Request::Remove { ids, delete_file: delete },
        Cmd::Move { ids, queue } => Request::Move { ids, queue },
        Cmd::Limit { rate: None, id: None } => {
            let mut client = remote::connect(&home, true).await?;
            let settings: Settings = remote::fetch(&mut client, &Request::Settings).await?;
            match settings.speed_limit {
                0 => println!("no speed limit"),
                rate => println!("speed limit {}/s for all downloads", human(rate as f64)),
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Limit { rate: None, id: Some(_) } => return Err("give a rate, for example: limit --id 3 500K".into()),
        Cmd::Limit { rate: Some(rate), id } => {
            let rate = parse_rate(&rate)?;
            match id {
                Some(id) => Request::DownloadLimit { id, rate },
                None => Request::SpeedLimit { rate },
            }
        }
        Cmd::Connections { connections } => Request::Connections { connections },
        Cmd::Categories { name: Some(name), dir: Some(dir) } => {
            let dir = std::path::absolute(dir).map_err(|e| e.to_string())?;
            Request::CategoryDir { name, dir }
        }
        Cmd::Categories { .. } => {
            let mut client = remote::connect(&home, true).await?;
            let categories: Vec<Category> = remote::fetch(&mut client, &Request::Categories).await?;
            for c in categories {
                println!("{:<11} {:<40} {}", c.name, c.dir.display(), c.extensions.join(" "));
            }
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Queue { command } => match command.unwrap_or(QueueCmd::List) {
            QueueCmd::List => {
                let mut client = remote::connect(&home, true).await?;
                let queues: Vec<Queue> = remote::fetch(&mut client, &Request::Queues).await?;
                let list: Vec<Download> = remote::fetch(&mut client, &Request::List).await?;
                remote::print_queues(&queues, &list);
                return Ok(ExitCode::SUCCESS);
            }
            QueueCmd::New { name } => Request::NewQueue { name },
            QueueCmd::Start { id } => Request::StartQueue { id },
            QueueCmd::Stop { id } => Request::StopQueue { id },
            QueueCmd::Set { id, name, parallel, retries, start, stop, days } => {
                let mut client = remote::connect(&home, true).await?;
                let queues: Vec<Queue> = remote::fetch(&mut client, &Request::Queues).await?;
                let mut q = queues.into_iter().find(|q| q.id == id).ok_or(format!("no queue with id {id}"))?;
                let time = |t: String| (!t.eq_ignore_ascii_case("off")).then_some(t);
                q.name = name.unwrap_or(q.name);
                q.parallel = parallel.unwrap_or(q.parallel);
                q.retries = retries.unwrap_or(q.retries);
                if let Some(start) = start {
                    q.schedule.start = time(start);
                }
                if let Some(stop) = stop {
                    q.schedule.stop = time(stop);
                }
                if let Some(days) = days {
                    q.schedule.days = parse_days(&days).ok_or(format!("not a day list: {days} (mon,tue,... or all)"))?;
                }
                remote::call(&mut client, &Request::UpdateQueue(q)).await?;
                return Ok(ExitCode::SUCCESS);
            }
        },
    };
    let mut client = remote::connect(&home, true).await?;
    let answer = remote::call(&mut client, &request).await?;
    if let Some(id) = answer.get("id").filter(|_| matches!(request, Request::Add(_) | Request::NewQueue { .. })) {
        println!("added with id {id}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Request headers from --referer, --cookie and -H "Name: value".
fn request_headers(
    referer: Option<String>,
    cookie: Option<String>,
    headers: Vec<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut all = Vec::new();
    all.extend(referer.map(|r| ("Referer".to_owned(), r)));
    all.extend(cookie.map(|c| ("Cookie".to_owned(), c)));
    for h in headers {
        let (k, v) = h.split_once(':').ok_or(format!("header needs \"Name: value\": {h}"))?;
        all.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    Ok(all)
}
