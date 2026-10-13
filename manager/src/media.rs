//! Joins a video stream and an audio stream into one file with ffmpeg, and turns a stream
//! downloaded as pieces into a normal file. Streams are copied packet for packet
//! (`-c copy`), never re-encoded, so there is no quality loss.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use engine::CancellationToken;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{Error, Result};

/// Container that holds both streams as they are, by the downloaded files' extensions:
/// MP4 for MP4 video with M4A/MP4 audio, WebM for WebM with WebM, Matroska for any other
/// pair (it takes every codec).
pub fn container(video: &Path, audio: &Path) -> &'static str {
    let ext = |p: &Path| p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match (ext(video).as_str(), ext(audio).as_str()) {
        ("mp4", "m4a" | "mp4") => "mp4",
        ("webm", "webm") => "webm",
        _ => "mkv",
    }
}

/// Writes `video`'s first video stream and `audio`'s first audio stream to `out`, whose
/// extension picks the container (see `container`). `out` appears only when complete and
/// read back (see `keep_if_readable`); until then the file is at `partial_for(video)`,
/// which must be on `out`'s drive, since the file is renamed. `stop` ends it early (see
/// `output`); nothing is left of the join then.
pub async fn merge(ffmpeg: &Path, video: &Path, audio: &Path, out: &Path, stop: &CancellationToken) -> Result<()> {
    join(ffmpeg, &[video, audio], &["0:v:0", "1:a:0"], out, stop).await
}

/// Copies every video and audio stream of `input` (joined HLS or DASH pieces: MPEG-TS or
/// fragmented MP4) into a normal file `out`, whose extension picks the container. `out`
/// appears, and `stop` ends it, as with `merge`.
pub async fn remux(ffmpeg: &Path, input: &Path, out: &Path, stop: &CancellationToken) -> Result<()> {
    join(ffmpeg, &[input], &["0:v?", "0:a?"], out, stop).await
}

async fn join(ffmpeg: &Path, inputs: &[&Path], maps: &[&str], out: &Path, stop: &CancellationToken) -> Result<()> {
    let format = match out.extension().and_then(|e| e.to_str()) {
        Some("mp4" | "m4a") => "mp4",
        Some("webm") => "webm",
        Some("mkv") => "matroska",
        _ => return Err(Error::Invalid(format!("no container for {}", out.display()))),
    };
    let partial = partial_for(inputs[0]);
    let mut cmd = command(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    for input in inputs {
        cmd.arg("-i").arg(input);
    }
    for map in maps {
        cmd.args(["-map", map]);
    }
    cmd.args(["-c", "copy"]);
    if format == "mp4" {
        // Index at the front, so players can start before reading the whole file.
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.args(["-f", format]).arg(&partial);
    let written = output(cmd, stop).await.and_then(|output| match output.status.success() {
        true => Ok(()),
        false => Err(Error::Tool(format!("ffmpeg could not join the streams: {}", String::from_utf8_lossy(&output.stderr).trim()))),
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    keep_if_readable(ffmpeg, &partial, out, stop).await
}

/// Where a join of `input` with others, or of `input` alone, is written until it is done:
/// next to that stream and under its name, so that what clears a download's streams clears
/// what is left of a join too (the program was closed by force meanwhile, or the file could
/// not be removed).
pub(crate) fn partial_for(input: &Path) -> PathBuf {
    PathBuf::from(format!("{}.merging", input.display()))
}

/// Gives a file ffmpeg wrote its final name if ffmpeg can open it again, and removes it if
/// not. The exit code of the writing run does not say: a build without the VP9 decoder
/// wrote MP4 files from VP9 video that nothing could open, and reported success.
async fn keep_if_readable(ffmpeg: &Path, partial: &Path, out: &Path, stop: &CancellationToken) -> Result<()> {
    let kept = async {
        let said = header(ffmpeg, partial, stop).await?;
        // The line of an opened file is "Input #0, <container>, from '<file>':"; both
        // halves, since the file's own name is in the lines of a failure too.
        if !said.lines().any(|line| line.contains("Input #0,") && line.contains(", from '")) {
            return Err(Error::Tool(format!("ffmpeg wrote a file it cannot read: {}", said.trim())));
        }
        Ok(std::fs::rename(partial, out)?)
    };
    let kept = kept.await;
    if kept.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    kept
}

/// What ffmpeg says of `file`'s header: "Input #0, ..." with the length and the streams
/// when it can open the file, else why it cannot. Without an output ffmpeg reads no
/// further than the header (and exits with an error either way).
pub(crate) async fn header(ffmpeg: &Path, file: &Path, stop: &CancellationToken) -> Result<String> {
    let mut cmd = command(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-i"]).arg(file);
    Ok(String::from_utf8_lossy(&output(cmd, stop).await?.stderr).into_owned())
}

/// Runs ffmpeg to its end, as `Command::output` does, unless `stop` comes first (the
/// download was stopped): that ends it as `Cancelled`. Ended early, by `stop` or an error,
/// ffmpeg is killed and this returns once it is gone: Windows deletes no file a program
/// still has open, and a stopped download's files may be removed at once.
async fn output(mut cmd: Command, stop: &CancellationToken) -> Result<Output> {
    let mut child = cmd.spawn()?;
    let (mut out, mut err) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let ended = tokio::select! {
        ended = async { tokio::try_join!(child.wait(), out.read_to_end(&mut stdout), err.read_to_end(&mut stderr)) } => ended.map_err(Error::from),
        () = stop.cancelled() => Err(engine::Error::Cancelled.into()),
    };
    match ended {
        Ok((status, ..)) => Ok(Output { status, stdout, stderr }),
        Err(e) => {
            let _ = child.start_kill();
            // Gone a few milliseconds after the kill; the limit is for a kill that failed.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            Err(e)
        }
    }
}

/// ffmpeg ships with RDM: the one beside the running program. `RDM_FFMPEG` names another,
/// for work on the code, where the programs run from the build folder.
pub fn shipped_ffmpeg() -> PathBuf {
    let name = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    let beside = || std::env::current_exe().ok().map(|me| me.with_file_name(&name));
    std::env::var_os("RDM_FFMPEG").map(PathBuf::from).or_else(beside).unwrap_or_else(|| PathBuf::from(&name))
}

/// ffmpeg without a console window popping up next to the app.
pub(crate) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// MD5 and size of every packet of `stream` (for example `v:0`), in order. Equal lists for
/// a source and a joined file prove the stream was copied bit for bit.
///
/// AAC in MPEG-TS carries a 7-byte ADTS header on every packet, which MP4 keeps once in its
/// own header instead; `strip_adts` removes those headers from the source's packets
/// (aac_adtstoasc) so the audio data itself is compared.
pub async fn packet_hashes(ffmpeg: &Path, file: &Path, stream: &str, strip_adts: bool) -> Result<Vec<String>> {
    let mut cmd = command(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", &format!("0:{stream}"), "-c", "copy"]);
    if strip_adts {
        cmd.args(["-bsf:a", "aac_adtstoasc"]);
    }
    let output = cmd.args(["-f", "framemd5", "-"]).output().await?;
    if !output.status.success() {
        return Err(Error::Tool(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    // Lines are "stream, dts, pts, duration, size, md5", then side data such as MPEG-TS
    // stream ids; timestamps may be shifted by the container, the bytes may not.
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let fields: Vec<&str> = l.split(',').map(str::trim).collect();
            (fields.len() >= 6).then(|| format!("{} {}", fields[4], fields[5]))
        })
        .collect())
}

/// Where the tests look for the ffmpeg RDM ships: where `RDM_FFMPEG` points, else the build
/// in `target/ffmpeg-min` (see `ffmpeg/README.md`).
#[cfg(test)]
pub(crate) fn ffmpeg_in_tests() -> PathBuf {
    let built = match (cfg!(windows), std::env::consts::ARCH) {
        (true, _) => "ffmpeg-win64/ffmpeg.exe",
        (false, "aarch64") => "ffmpeg-macos-arm64/ffmpeg",
        (false, _) => "ffmpeg-macos-x64/ffmpeg",
    };
    std::env::var_os("RDM_FFMPEG")
        .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/ffmpeg-min/work").join(built), PathBuf::from)
}

/// The ffmpeg RDM ships, for the tests of what RDM does with it. Without one the test
/// fails, rather than pass having checked nothing.
#[cfg(test)]
pub(crate) fn ffmpeg_for_tests() -> PathBuf {
    let ffmpeg = ffmpeg_in_tests();
    assert!(ffmpeg.is_file(), "this test needs the ffmpeg RDM ships: build it (ffmpeg/README.md) or set RDM_FFMPEG to its path (no {})", ffmpeg.display());
    ffmpeg
}

/// A full ffmpeg, for what the shipped one cannot do: it has no encoders to make a sample
/// video with, and no packet hashes. `RDM_FFMPEG_FULL` names it, else the `ffmpeg` on the
/// PATH is taken.
#[cfg(test)]
pub(crate) fn full_ffmpeg_for_tests() -> PathBuf {
    let ffmpeg = std::env::var_os("RDM_FFMPEG_FULL").map_or_else(|| PathBuf::from("ffmpeg"), PathBuf::from);
    let runs = std::process::Command::new(&ffmpeg).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status();
    assert!(
        runs.is_ok_and(|status| status.success()),
        "this test makes its sample videos with a full ffmpeg: put one on the PATH or set RDM_FFMPEG_FULL to its path (tried {})",
        ffmpeg.display()
    );
    ffmpeg
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn container_keeps_both_streams_as_they_are() {
        let c = |v: &str, a: &str| container(Path::new(v), Path::new(a));
        assert_eq!(c("x.f299.mp4", "x.f140.m4a"), "mp4");
        assert_eq!(c("x.f303.webm", "x.f251.webm"), "webm");
        assert_eq!(c("x.f299.mp4", "x.f251.webm"), "mkv");
        assert_eq!(c("x.f303.webm", "x.f140.m4a"), "mkv");
    }

    /// Needs both ffmpegs (`ffmpeg_for_tests`, `full_ffmpeg_for_tests`). Makes a short video
    /// and audio file with the full one's encoders, joins them with the shipped one, and
    /// checks every packet came through unchanged.
    #[tokio::test]
    async fn merge_copies_every_packet_unchanged() {
        let (ffmpeg, full) = (ffmpeg_for_tests(), full_ffmpeg_for_tests());
        let tmp = testserver::Folder::new("merge");
        let dir = tmp.path().to_owned();
        let (video, audio) = (dir.join("v.mp4"), dir.join("a.m4a"));
        let make = |args: &[&str], out: &Path| {
            let mut cmd = command(&full);
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]).args(args).arg(out);
            cmd
        };
        let status = make(&["-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25", "-c:v", "mpeg4"], &video).status().await.unwrap();
        assert!(status.success());
        let status = make(&["-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-c:a", "aac"], &audio).status().await.unwrap();
        assert!(status.success());

        let out = dir.join(format!("joined.{}", container(&video, &audio)));
        let never = CancellationToken::new();
        merge(&ffmpeg, &video, &audio, &out, &never).await.unwrap();
        assert!(!partial_for(&video).exists());
        let v = packet_hashes(&full, &video, "v:0", false).await.unwrap();
        let a = packet_hashes(&full, &audio, "a:0", false).await.unwrap();
        assert!(v.len() > 50 && a.len() > 50, "{} video, {} audio packets", v.len(), a.len());
        assert_eq!(packet_hashes(&full, &out, "v:0", false).await.unwrap(), v);
        assert_eq!(packet_hashes(&full, &out, "a:0", false).await.unwrap(), a);

        // The join is written at `partial_for` the video: with a folder in that place it
        // cannot be.
        std::fs::create_dir(partial_for(&video)).unwrap();
        let second = dir.join("second.mp4");
        assert!(matches!(merge(&ffmpeg, &video, &audio, &second, &never).await, Err(Error::Tool(_))));
        assert!(!second.exists());
    }

    /// Needs a full ffmpeg (`full_ffmpeg_for_tests`), for a run that takes long. A run stopped
    /// part-way ends as cancelled and leaves no ffmpeg behind that holds its file: the file
    /// can be deleted at once. (Windows refuses while a program has the file open; elsewhere
    /// the delete passes either way.)
    #[tokio::test]
    async fn a_stopped_run_has_let_go_of_its_file() {
        let ffmpeg = full_ffmpeg_for_tests();
        let tmp = testserver::Folder::new("stopped-run");
        let file = tmp.path().join("long.mp4");
        for _ in 0..5 {
            // Ten minutes of video to encode: far from done when it is stopped.
            let mut cmd = command(&ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
                .args(["-f", "lavfi", "-i", "testsrc=duration=600:size=320x240:rate=25", "-c:v", "mpeg4", "-f", "mp4"])
                .arg(&file);
            let stop = CancellationToken::new();
            let mut run = std::pin::pin!(output(cmd, &stop));
            let deadline = Instant::now() + Duration::from_secs(20);
            while !file.is_file() {
                assert!(Instant::now() < deadline, "ffmpeg wrote nothing");
                assert!(tokio::time::timeout(Duration::from_millis(5), &mut run).await.is_err(), "ffmpeg ended early");
            }
            stop.cancel();
            assert!(matches!(run.await, Err(Error::Download(engine::Error::Cancelled))));
            std::fs::remove_file(&file).unwrap();
        }
    }

    /// Needs both ffmpegs (`ffmpeg_for_tests`, `full_ffmpeg_for_tests`). A written file gets
    /// its final name when ffmpeg opens it; one it cannot open (an MP4 cut off before its
    /// index, which is written last) is removed instead, whatever the run that wrote it
    /// reported.
    #[tokio::test]
    async fn a_written_file_is_kept_only_if_ffmpeg_opens_it() {
        let (ffmpeg, full) = (ffmpeg_for_tests(), full_ffmpeg_for_tests());
        let tmp = testserver::Folder::new("read-back");
        let dir = tmp.path().to_owned();
        let (partial, out) = (dir.join("clip.mp4.merging"), dir.join("clip.mp4"));
        let status = command(&full)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25", "-c:v", "mpeg4", "-f", "mp4"])
            .arg(&partial)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let never = CancellationToken::new();
        keep_if_readable(&ffmpeg, &partial, &out, &never).await.unwrap();
        assert!(out.is_file() && !partial.exists());

        // The second name has what ffmpeg prints of an opened file in it.
        let whole = std::fs::read(&out).unwrap();
        for name in ["cut.mp4", "Input #0, cut.mp4"] {
            let (partial, cut) = (dir.join(format!("{name}.merging")), dir.join(name));
            std::fs::write(&partial, &whole[..whole.len() / 2]).unwrap();
            let error = keep_if_readable(&ffmpeg, &partial, &cut, &never).await.unwrap_err();
            assert!(matches!(&error, Error::Tool(text) if text.starts_with("ffmpeg wrote a file it cannot read: ")), "{error}");
            assert!(!partial.exists() && !cut.exists(), "{name}");
        }

        // A file that opens but cannot get its name (a folder has it) is not left behind.
        std::fs::write(&partial, &whole).unwrap();
        let taken = dir.join("taken.mp4");
        std::fs::create_dir(&taken).unwrap();
        assert!(matches!(keep_if_readable(&ffmpeg, &partial, &taken, &never).await, Err(Error::Io(_))));
        assert!(!partial.exists());
    }

    /// Needs both ffmpegs (`ffmpeg_for_tests`, `full_ffmpeg_for_tests`). Joined HLS pieces
    /// are MPEG-TS or fragmented MP4; both become a normal MP4 with every packet unchanged
    /// (TS audio minus its ADTS headers).
    #[tokio::test]
    async fn remux_copies_every_packet_unchanged() {
        let (ffmpeg, full) = (ffmpeg_for_tests(), full_ffmpeg_for_tests());
        let tmp = testserver::Folder::new("remux");
        let dir = tmp.path().to_owned();
        let sources: [(&str, &[&str], bool); 2] = [
            ("pieces.ts", &["-f", "mpegts"], true),
            ("pieces.mp4", &["-f", "mp4", "-movflags", "frag_keyframe+empty_moov"], false),
        ];
        for (name, format, ts) in sources {
            let source = dir.join(name);
            let status = command(&full)
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
                .args(["-f", "lavfi", "-i", "testsrc=duration=3:size=320x240:rate=25"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=3"])
                .args(["-c:v", "mpeg4", "-c:a", "aac"])
                .args(format)
                .arg(&source)
                .status()
                .await
                .unwrap();
            assert!(status.success());
            let out = dir.join(format!("{name}.out.mp4"));
            remux(&ffmpeg, &source, &out, &CancellationToken::new()).await.unwrap();
            let v = packet_hashes(&full, &source, "v:0", false).await.unwrap();
            let a = packet_hashes(&full, &source, "a:0", ts).await.unwrap();
            assert!(v.len() > 50 && a.len() > 50, "{name}: {} video, {} audio packets", v.len(), a.len());
            assert_eq!(packet_hashes(&full, &out, "v:0", false).await.unwrap(), v, "{name} video");
            assert_eq!(packet_hashes(&full, &out, "a:0", false).await.unwrap(), a, "{name} audio");
        }
    }
}
