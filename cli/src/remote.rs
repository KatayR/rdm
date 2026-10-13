//! Commands that go to the background manager, which is started when it is not running.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use manager::ipc::{Client, Request, is_other_version};
use manager::model::{Download, Queue, Status};
use manager::schedule::days_text;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::fmt::{clock, human};

/// Connects to the daemon, starting it in the background first if nobody answers.
pub async fn connect(home: &Path, start: bool) -> Result<Client, String> {
    match Client::connect(home).await {
        Ok(client) => return Ok(client),
        // Somebody answers, as another version: starting a manager would not change that.
        Err(e) if is_other_version(&e) => return Err(e.to_string()),
        Err(_) => {}
    }
    if !start {
        return Err("the manager is not running".into());
    }
    start_daemon(home).map_err(|e| format!("could not start the manager: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match Client::connect(home).await {
            Ok(client) => return Ok(client),
            Err(e) if is_other_version(&e) => return Err(e.to_string()),
            Err(_) if Instant::now() < deadline => {}
            Err(e) => return Err(format!("the manager did not start ({e}); see {}", home.join("daemon.log").display())),
        }
    }
}

/// The manager as a detached process, logging to daemon.log.
fn start_daemon(home: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(home.join("daemon.log"))?;
    let mut cmd = daemon_command(home)?;
    cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        // Windows hands every inheritable handle to the child. The daemon outlives this
        // process, so if it held our stdout, a pipe reading it (`rdm list | more`) would
        // never end. This process exits soon, so its own handles can stop being inheritable.
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own std handles; a missing or
            // invalid handle only makes the call fail, which is fine.
            unsafe {
                SetHandleInformation(GetStdHandle(which), HANDLE_FLAG_INHERIT, 0);
            }
        }
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

/// What starts the manager on the data folder `home`: the app (tray, windows) when it is
/// installed next to this binary, else the bare daemon (the browser helper is another
/// binary there). It is told the folder, so it serves the one it was started for whatever
/// its own surroundings say.
fn daemon_command(home: &Path) -> std::io::Result<Command> {
    let here = std::env::current_exe()?;
    let app = here.with_file_name(format!("rdm-app{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = if app.exists() {
        let mut cmd = Command::new(app);
        cmd.arg("--background");
        cmd
    } else {
        let mut cmd = Command::new(here.with_file_name(format!("rdm{}", std::env::consts::EXE_SUFFIX)));
        cmd.arg("daemon");
        cmd
    };
    cmd.env("RDM_HOME", home);
    Ok(cmd)
}

pub async fn call(client: &mut Client, request: &Request) -> Result<Value, String> {
    client.call(request).await.map_err(|e| format!("lost the manager: {e}"))?
}

pub async fn fetch<T: DeserializeOwned>(client: &mut Client, request: &Request) -> Result<T, String> {
    serde_json::from_value(call(client, request).await?).map_err(|e| format!("unexpected answer: {e}"))
}

pub fn print_downloads(list: &[Download]) {
    if list.is_empty() {
        println!("no downloads");
        return;
    }
    println!("{:>4}  {:<11}  {:>6}  {:>10}  {:>11}  {:>4}  {:>7}  NAME", "ID", "STATUS", "DONE", "SIZE", "SPEED", "CONN", "LEFT");
    for d in list {
        let done = match d.size {
            Some(size) if size > 0 => format!("{:.1}%", d.downloaded as f64 * 100.0 / size as f64),
            _ if d.downloaded > 0 => human(d.downloaded as f64),
            _ => "-".into(),
        };
        let running = d.status == Status::Downloading;
        let left = match (running, d.size) {
            (true, Some(size)) if d.speed > 0 => clock(size.saturating_sub(d.downloaded) as f64 / d.speed as f64),
            _ => "-".into(),
        };
        println!(
            "{:>4}  {:<11}  {:>6}  {:>10}  {:>11}  {:>4}  {:>7}  {}",
            d.id,
            d.status.as_str(),
            done,
            d.size.map_or("?".into(), |s| format!("{}{}", if d.size_estimated { "~" } else { "" }, human(s as f64))),
            if running { format!("{}/s", human(d.speed as f64)) } else { "-".into() },
            if running { d.active_connections.to_string() } else { "-".into() },
            left,
            d.file_name.as_deref().unwrap_or(&d.url),
        );
        if let (Status::Failed | Status::Waiting, Some(error)) = (d.status, &d.error) {
            println!("{:>6}last error: {error}", "");
        }
        if let (Status::Complete, Some(check)) = (d.status, &d.file_check)
            && !check.ok
        {
            println!("{:>6}may be damaged: {}", "", check.text);
        }
    }
}

pub fn print_queues(queues: &[Queue], list: &[Download]) {
    println!("{:>4}  {:<24}  {:<7}  {:>5}  {:>7}  {:>7}  SCHEDULE", "ID", "NAME", "STATE", "FILES", "AT ONCE", "RETRIES");
    for q in queues {
        let files = list.iter().filter(|d| d.queue == q.id && d.status != Status::Complete).count();
        let s = &q.schedule;
        let schedule = match (&s.start, &s.stop) {
            (None, None) => "-".to_owned(),
            (start, stop) => format!(
                "start {} stop {} ({})",
                start.as_deref().unwrap_or("-"),
                stop.as_deref().unwrap_or("-"),
                days_text(s.days)
            ),
        };
        println!(
            "{:>4}  {:<24}  {:<7}  {:>5}  {:>7}  {:>7}  {schedule}",
            q.id,
            q.name,
            if q.running { "running" } else { "stopped" },
            files,
            q.parallel,
            q.retries
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program that gets started is told which data folder to serve.
    #[test]
    fn a_started_manager_gets_the_data_folder() {
        let home = Path::new("some").join("data folder");
        let cmd = daemon_command(&home).unwrap();
        let told: Vec<_> = cmd.get_envs().collect();
        assert_eq!(told, [(std::ffi::OsStr::new("RDM_HOME"), Some(home.as_os_str()))]);
    }
}
