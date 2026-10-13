//! A video download: yt-dlp's streams fetched one by one next to the final file and
//! joined (or remuxed) by ffmpeg, with progress for the whole video.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine::{CancellationToken, Progress};
use tokio::sync::watch;

use super::Inner;
use super::files::{remove_if_exists, stem_of};
use super::run::{Finished, Job};
use crate::media;
use crate::video::{self, Delivery};
use crate::caught;
use crate::{Error, Result};

/// Where one stream of a video comes from, once looked up.
enum Source {
    File(engine::Probe),
    Pieces(engine::Pieces),
}

impl Source {
    fn expected_size(&self) -> Option<u64> {
        match self {
            Source::File(probe) => probe.size,
            Source::Pieces(list) => list.expected_size(),
        }
    }

    /// Whether `expected_size` is a guess.
    fn estimated(&self) -> bool {
        matches!(self, Source::Pieces(list) if list.exact_size().is_none())
    }

    fn pieces(&self) -> usize {
        match self {
            Source::File(_) => 0,
            Source::Pieces(list) => list.count(),
        }
    }
}

/// A video page or a caught playlist: yt-dlp gives fresh stream addresses (they expire, so
/// on every start), or the playlists saved with the download name them (picked from a
/// playlist the browser's page read); the engine downloads each stream next to the final
/// file (plain files in segments, playlists piece by piece), and ffmpeg joins video and
/// audio, or turns a playlist's pieces into a normal file, by copying the streams unchanged.
/// Progress covers all streams as one file.
pub(super) async fn transfer_video(
    inner: &Arc<Inner>,
    id: i64,
    job: &Job,
    format: &str,
    progress: &watch::Sender<Progress>,
) -> Result<Finished> {
    let playlists = inner.store().playlists(id)?;
    let resolved = match playlists.is_empty() {
        true => job.unless_stopped(video::resolve(&inner.tools, &inner.listings, &job.url, &job.headers, format)).await?,
        false => caught::resolve(&playlists, &job.headers, format)?,
    };
    let needs_ffmpeg = resolved.streams.len() > 1 || resolved.streams.iter().any(|s| s.delivery != Delivery::File);
    let ffmpeg = match needs_ffmpeg {
        false => None,
        true => Some(inner.ffmpeg().ok_or_else(|| Error::Tool(format!("ffmpeg is missing: it belongs at {}", inner.ffmpeg.display())))?),
    };
    let mut sources = Vec::new();
    for s in &resolved.streams {
        let source = job
            .unless_stopped(async {
                Ok(match &s.delivery {
                    Delivery::File => Source::File(engine::probe(&s.url, &s.headers).await?),
                    // A playlist the page read is not asked for again.
                    Delivery::Hls => Source::Pieces(match caught::text_of(&playlists, &s.url) {
                        Some(text) => engine::hls::read(text, &s.url, &s.headers)?,
                        None => engine::hls::load(&s.url, &s.headers).await?,
                    }),
                    Delivery::Dash(pieces) => Source::Pieces(engine::Pieces::new(&s.url, &s.headers, pieces.clone())?),
                })
            })
            .await?;
        sources.push(match source {
            Source::Pieces(mut list) => {
                list.size_hint = s.size_hint;
                Source::Pieces(list)
            }
            file => file,
        });
    }
    // yt-dlp's length, else the longest playlist's (the sum of its pieces' lengths).
    let length = resolved.duration.or_else(|| {
        let pieces = sources.iter().map(|s| if let Source::Pieces(list) = s { list.duration() } else { 0.0 });
        Some(pieces.fold(0.0, f64::max)).filter(|l| *l > 0.0)
    });
    let sizes: Vec<Option<u64>> = sources.iter().map(Source::expected_size).collect();
    let guessed: Vec<bool> = sources.iter().map(Source::estimated).collect();
    let counts: Vec<usize> = sources.iter().map(Source::pieces).collect();
    let total = sizes.iter().copied().sum::<Option<u64>>();
    let Some((name, dir)) = inner.place(id, &resolved.file_name(), total, guessed.contains(&true)) else { return Err(engine::Error::Cancelled.into()) };
    let stem = stem_of(&name);

    let mut parts = Vec::new();
    let mut at = Offset {
        bytes_before: 0,
        bytes_after: None,
        after_estimated: false,
        pieces_before: 0,
        pieces_all: counts.iter().sum(),
    };
    for (i, (s, source)) in resolved.streams.iter().zip(sources).enumerate() {
        let part = stream_part(&stem, id, &s.format_id, &s.ext);
        adopt_old_part(&dir, &format!("{stem}.f{}.{}", s.format_id, s.ext), &part)?;
        at.bytes_after = sizes[i + 1..].iter().copied().sum::<Option<u64>>();
        at.after_estimated = guessed[i + 1..].contains(&true);
        // A stream finished in an earlier run of this download is there under its own
        // name, with no `.part` file left beside it.
        let done = dir.join(&part);
        let path = if done.is_file() && !dir.join(format!("{part}.part")).exists() {
            done
        } else {
            let opts = job.options(inner, dir.clone(), part, Vec::new());
            let (tx, rx) = watch::channel(Progress::default());
            let forward = forward_progress(rx, progress, &at);
            tokio::select! {
                out = async {
                    match source {
                        Source::File(probe) => engine::download_probed(probe, &opts, &tx, &job.stop).await,
                        Source::Pieces(list) => engine::download_pieces(list, &opts, &tx, &job.stop).await,
                    }
                } => out?.path,
                () = forward => unreachable!("forwarding ends only with the download"),
            }
        };
        at.bytes_before += std::fs::metadata(&path)?.len();
        at.pieces_before += counts[i];
        parts.push(path);
    }

    let out = match ffmpeg {
        Some(ffmpeg) => write_media(&ffmpeg, &parts, &dir, &name, &job.stop).await?,
        None => {
            let out = engine::free_path(&dir, &name);
            std::fs::rename(&parts[0], &out)?;
            parts.clear();
            out
        }
    };
    for p in &parts {
        remove_if_exists(p)?;
    }
    let size = std::fs::metadata(&out)?.len();
    Ok(Finished { path: out, size, checked: None, video_length: length })
}

/// Joins video and audio `parts`, or remuxes a single one, into a free path for
/// `dir/name`. When that container cannot hold the streams as they are (an audio codec MP4
/// does not take, say), Matroska is used instead, since it takes every codec; nothing is
/// ever re-encoded. `stop` ends it as cancelled, with ffmpeg gone and nothing left of the
/// join (see `media::merge`).
async fn write_media(ffmpeg: &Path, parts: &[PathBuf], dir: &Path, name: &str, stop: &CancellationToken) -> Result<PathBuf> {
    let write = async |out: &Path| match parts {
        [video, audio] => media::merge(ffmpeg, video, audio, out, stop).await,
        [single] => media::remux(ffmpeg, single, out, stop).await,
        _ => Err(Error::Tool(format!("{} streams to join", parts.len()))),
    };
    let stopped = |e: &Error| matches!(e, Error::Download(engine::Error::Cancelled));
    let out = engine::free_path(dir, name);
    let first = match write(&out).await {
        Ok(()) => return Ok(out),
        Err(e) => e,
    };
    if stopped(&first) || out.extension().is_some_and(|e| e == "mkv") {
        return Err(first);
    }
    let mkv = engine::free_path(dir, &format!("{}.mkv", stem_of(name)));
    match write(&mkv).await {
        Ok(()) => Ok(mkv),
        Err(second) if stopped(&second) => Err(second),
        Err(_) => Err(first),
    }
}

/// Where one stream sits in the whole video.
struct Offset {
    /// Bytes of earlier streams, all on disk.
    bytes_before: u64,
    /// Expected bytes of later streams, and whether that is a guess.
    bytes_after: Option<u64>,
    after_estimated: bool,
    /// Pieces of earlier playlist streams, and of all of them.
    pieces_before: usize,
    pieces_all: usize,
}

/// Shows one stream's progress as part of the whole video: earlier streams are counted as
/// done, later ones are still to come, and segments are moved to the stream's place.
async fn forward_progress(mut rx: watch::Receiver<Progress>, to: &watch::Sender<Progress>, at: &Offset) {
    let before = at.bytes_before;
    while rx.changed().await.is_ok() {
        let p = rx.borrow().clone();
        let shift = |s: engine::Segment| engine::Segment { start: s.start + before, pos: s.pos + before, end: s.end + before };
        let written = p.pieces.map_or(0, |(w, _)| w);
        to.send_replace(Progress {
            downloaded: before + p.downloaded,
            total: p.total.zip(at.bytes_after).map(|(t, a)| before + t + a),
            estimated: p.estimated || at.after_estimated,
            connections: p.connections,
            segments: p.segments.into_iter().map(shift).collect(),
            pieces: (at.pieces_all > 0).then_some((at.pieces_before + written, at.pieces_all)),
        });
    }
    std::future::pending().await
}

/// The file one stream of video download `id` is fetched into, next to the joined file.
/// The id keeps the streams of two downloads with the same name apart (ids are never reused).
fn stream_part(stem: &str, id: i64, format_id: &str, ext: &str) -> String {
    format!("{}{format_id}.{ext}", stream_part_prefix(stem, id))
}

fn stream_part_prefix(stem: &str, id: i64) -> String {
    format!("{stem}.d{id}.f")
}

/// Every stream part of download `id`: finished, `.part` and resume state, and a join of
/// them ffmpeg did not finish (`media::partial_for`).
pub(super) fn remove_stream_parts(dir: &Path, stem: &str, id: i64) -> Result<()> {
    let prefix = stream_part_prefix(stem, id);
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            remove_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

/// Before stream parts carried the download id they were named `old`: an unfinished one
/// (`.part` with its resume state) moves to the new name, so a paused download keeps its
/// progress; the engine still checks it on resume. A finished one is left alone, since it
/// may be another download's.
fn adopt_old_part(dir: &Path, old: &str, new: &str) -> Result<()> {
    let (old_part, old_state) = (dir.join(format!("{old}.part")), dir.join(format!("{old}.part.json")));
    let new_part = dir.join(format!("{new}.part"));
    if old_part.is_file() && old_state.is_file() && !new_part.exists() {
        std::fs::rename(&old_part, &new_part)?;
        std::fs::rename(&old_state, dir.join(format!("{new}.part.json")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_parts_under_the_old_name_are_taken_over() {
        let tmp = testserver::Folder::new("old-parts");
        let dir = tmp.path().to_owned();
        let new = stream_part("clip", 35, "hls-1", "mp4");
        assert_eq!(new, "clip.d35.fhls-1.mp4");
        for f in ["clip.fhls-1.mp4.part", "clip.fhls-1.mp4.part.json", "clip.f140.m4a"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        adopt_old_part(&dir, "clip.fhls-1.mp4", &new).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(format!("{new}.part"))).unwrap(), "clip.fhls-1.mp4.part");
        assert_eq!(std::fs::read_to_string(dir.join(format!("{new}.part.json"))).unwrap(), "clip.fhls-1.mp4.part.json");
        assert!(!dir.join("clip.fhls-1.mp4.part").exists());
        // A finished part under the old name may be another download's: left where it is.
        let audio = stream_part("clip", 35, "140", "m4a");
        adopt_old_part(&dir, "clip.f140.m4a", &audio).unwrap();
        assert!(dir.join("clip.f140.m4a").exists() && !dir.join(&audio).exists());
    }

    /// A join ffmpeg did not finish (the program was closed by force meanwhile) goes with
    /// the download's streams; another download's stays.
    #[test]
    fn an_unfinished_join_is_removed_with_the_streams() {
        let tmp = testserver::Folder::new("unfinished-join");
        let dir = tmp.path().to_owned();
        let join_of = |id| media::partial_for(&dir.join(stream_part("clip", id, "299", "mp4")));
        for id in [3, 35] {
            std::fs::write(join_of(id), "half a file").unwrap();
        }
        remove_stream_parts(&dir, "clip", 3).unwrap();
        assert!(!join_of(3).exists() && join_of(35).exists());
    }

    /// Needs both ffmpegs (`media::ffmpeg_for_tests`, `media::full_ffmpeg_for_tests`). A join
    /// stopped part-way ends as cancelled and has removed what it wrote, in the first try and
    /// in the one as Matroska (MP4 does not take raw video); the stream is still there, and
    /// goes with the download.
    #[tokio::test]
    async fn a_stopped_join_leaves_only_the_streams() {
        use std::time::{Duration, Instant};
        let ffmpeg = media::ffmpeg_for_tests();
        let tmp = testserver::Folder::new("stopped-join");
        let dir = tmp.path().to_owned();
        // 400 MB of raw video: a join of it is far from done when it is stopped.
        let part = dir.join(stream_part("clip", 7, "620", "mkv"));
        let status = media::command(&media::full_ffmpeg_for_tests())
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=size=1280x720:rate=30", "-frames:v", "300", "-c:v", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg(&part)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let left = || std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
        // The first try as MP4 writes nothing before it fails: a megabyte is the second's.
        for (name, written) in [("clip.mkv", 0), ("clip.mp4", 1 << 20)] {
            let stop = CancellationToken::new();
            let mut join = std::pin::pin!(write_media(&ffmpeg, std::slice::from_ref(&part), &dir, name, &stop));
            let deadline = Instant::now() + Duration::from_secs(20);
            while !std::fs::metadata(media::partial_for(&part)).is_ok_and(|file| file.len() >= written) {
                assert!(Instant::now() < deadline, "{name}: ffmpeg wrote nothing");
                assert!(tokio::time::timeout(Duration::from_millis(5), &mut join).await.is_err(), "{name}: joined early");
            }
            stop.cancel();
            let ended = join.await;
            assert!(matches!(ended, Err(Error::Download(engine::Error::Cancelled))), "{name}: {ended:?}");
            assert_eq!(left(), [part.file_name().unwrap()], "{name}");
        }
        remove_stream_parts(&dir, "clip", 7).unwrap();
        assert!(left().is_empty());
    }

    /// Waits until download `id` has failed and gives its error.
    async fn failure(m: &crate::Manager, id: i64) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while m.get(id).unwrap().status != crate::model::Status::Failed {
            assert!(std::time::Instant::now() < deadline, "{:?}", m.get(id).unwrap());
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        m.get(id).unwrap().error.unwrap()
    }

    /// A video picked from a playlist the browser's page read: the saved text names the
    /// pieces, so neither yt-dlp (there is none here) nor the playlist's address (which
    /// answers nothing here) is asked, also after a restart. The test server's pieces are no
    /// real video, so ffmpeg's step at the end fails; the stream is whole before it. Needs
    /// ffmpeg (`media::ffmpeg_for_tests`).
    #[tokio::test(flavor = "multi_thread")]
    async fn saved_playlists_are_downloaded_without_asking_again() {
        use crate::model::{NewDownload, Playlist};
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (dir, m, db) = super::super::testing::open("saved-playlists");
        media::ffmpeg_for_tests();

        let text = reqwest::get(server.url("/hls/index.m3u8?pieces=5&size=4000")).await.unwrap().text().await.unwrap();
        let answered_once = server.url("/hls/answered-once");
        let new = NewDownload {
            url: answered_once.clone(),
            video: Some("hls".into()),
            file_name: Some("clip.mp4".into()),
            playlists: vec![Playlist { url: answered_once.clone(), text: text.clone() }],
            ..NewDownload::default()
        };
        let id = m.add(new).unwrap().id;
        m.shutdown().await;
        drop(m);

        let m = super::super::testing::reopen(&dir);
        m.resume(id).unwrap();
        let error = failure(&m, id).await;
        assert!(error.starts_with("ffmpeg"), "{error}");
        let d = m.get(id).unwrap();
        assert_eq!(server.pieces_served(), 5);
        let stream = d.dir.unwrap().join(format!("clip.d{id}.fhls.mp4"));
        let whole: Vec<u8> = (0..5).flat_map(|n| server.piece(n, 4000, false)).collect();
        assert_eq!(std::fs::read(stream).unwrap(), whole);

        let saved = || db.query_row("SELECT count(*) FROM playlists WHERE download = ?1", [id], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(saved(), 1);
        m.remove(id, true).await.unwrap();
        assert_eq!(saved(), 0, "the playlists go with the download");
        m.shutdown().await;
    }

    /// The same for a DASH manifest the page read: its text names the pieces, its address
    /// (which answers nothing here) is not asked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_saved_dash_manifest_is_downloaded_from_its_text() {
        use crate::model::{NewDownload, Playlist};
        let server = testserver::start(([127, 0, 0, 1], 0).into(), 1 << 20).await.unwrap();
        let (_dir, m, _db) = super::super::testing::open("saved-manifest");
        media::ffmpeg_for_tests();

        // Five pieces of 2 s, numbered from 0, the picture with its sound in one stream.
        let text = r#"<MPD type="static" mediaPresentationDuration="PT10S"><Period>
            <AdaptationSet mimeType="video/mp4" codecs="avc1.64001f,mp4a.40.2">
              <SegmentTemplate duration="2" startNumber="0" media="piece?n=$Number$&amp;size=4000"/>
              <Representation id="v" bandwidth="16000" width="640" height="360"/>
            </AdaptationSet></Period></MPD>"#;
        let answered_once = server.url("/hls/answered-once.mpd");
        let new = NewDownload {
            url: answered_once.clone(),
            video: Some("d0".into()),
            file_name: Some("clip.mp4".into()),
            playlists: vec![Playlist { url: answered_once, text: text.to_owned() }],
            ..NewDownload::default()
        };
        let id = m.add(new).unwrap().id;
        m.resume(id).unwrap();
        let error = failure(&m, id).await;
        assert!(error.starts_with("ffmpeg"), "{error}");
        assert_eq!(server.pieces_served(), 5);
        let stream = m.get(id).unwrap().dir.unwrap().join(format!("clip.d{id}.fd0.mp4"));
        let whole: Vec<u8> = (0..5).flat_map(|n| server.piece(n, 4000, false)).collect();
        assert_eq!(std::fs::read(stream).unwrap(), whole);
        m.shutdown().await;
    }

    /// The audio playlist after a video file: sizes, pieces and the guess flag of the whole.
    #[tokio::test]
    async fn stream_progress_counts_for_the_whole_video() {
        let (tx, rx) = watch::channel(Progress::default());
        let (to, mut whole) = watch::channel(Progress::default());
        let at = Offset { bytes_before: 1000, bytes_after: Some(50), after_estimated: true, pieces_before: 4, pieces_all: 10 };
        let forward = tokio::spawn(async move { forward_progress(rx, &to, &at).await });
        tx.send_replace(Progress {
            downloaded: 200,
            total: Some(600),
            estimated: false,
            connections: 2,
            segments: vec![engine::Segment { start: 0, pos: 200, end: 600 }],
            pieces: Some((2, 6)),
        });
        whole.changed().await.unwrap();
        let p = whole.borrow().clone();
        forward.abort();
        assert_eq!((p.downloaded, p.total, p.estimated, p.connections), (1200, Some(1650), true, 2));
        assert_eq!(p.pieces, Some((6, 10)));
        assert_eq!((p.segments[0].start, p.segments[0].pos, p.segments[0].end), (1000, 1200, 1600));
    }
}
