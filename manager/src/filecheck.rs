//! Checks of a finished file by its type, for damage no hash covered: a zip archive's
//! contents are unpacked and their CRCs compared (as `unzip -t` does), and a video's length
//! is compared with what the site said. They only warn: the file stays complete.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use zip::result::ZipError;

use crate::media;
use crate::model::FileCheck;

/// Files that are zip archives under another name.
const ZIP_TYPES: &[&str] = &[
    "zip", "jar", "war", "apk", "aab", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "whl", "nupkg", "vsix", "xpi",
    "ipa", "appx", "msix",
];

/// The files the ffmpeg RDM ships can open (its demuxers, `ffmpeg/build_ffmpeg.sh`). Of any
/// other kind it says "Invalid data found", of a sound file as of a broken one, so a video
/// under another extension is not checked.
const FFMPEG_OPENS: &[&str] =
    &["mp4", "m4v", "m4a", "m4b", "mov", "3gp", "3g2", "mkv", "mka", "webm", "ts", "m2ts", "mts", "aac", "ac3", "eac3", "mp3"];

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Zip,
    /// A video download, with the length in seconds the site gave.
    Video { expected: f64 },
}

/// The check for a finished file, if one applies: videos by their known length (those
/// ffmpeg can open), archives by their extension.
pub fn kind(path: &Path, video_length: Option<f64>) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if let Some(expected) = video_length.filter(|l| *l > 0.0) {
        return FFMPEG_OPENS.contains(&ext.as_str()).then_some(Kind::Video { expected });
    }
    ZIP_TYPES.contains(&ext.as_str()).then_some(Kind::Zip)
}

/// Runs the check; None when it could not run (no ffmpeg, or ffmpeg found no length).
pub async fn run(kind: Kind, path: PathBuf, ffmpeg: Option<PathBuf>) -> Option<FileCheck> {
    match kind {
        Kind::Zip => tokio::task::spawn_blocking(move || test_zip(&path)).await.ok(),
        Kind::Video { expected } => video_length(&ffmpeg?, &path, expected).await,
    }
}

fn ok(text: String) -> FileCheck {
    FileCheck { ok: true, text }
}

fn damaged(text: String) -> FileCheck {
    FileCheck { ok: false, text }
}

/// Every entry unpacked to the end, where the zip crate compares its CRC.
fn test_zip(path: &Path) -> FileCheck {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => return damaged(format!("could not read the file ({e})")),
    };
    let mut archive = match zip::ZipArchive::new(BufReader::new(file)) {
        Ok(archive) => archive,
        Err(e) => return damaged(format!("not a readable zip archive ({e})")),
    };
    let (mut tested, mut untested) = (0, 0);
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(entry) => entry,
            // Another packing method, or a password: nothing to say about it.
            Err(ZipError::UnsupportedArchive(_)) => {
                untested += 1;
                continue;
            }
            Err(e) => return damaged(format!("entry {} of the zip archive is damaged ({e})", i + 1)),
        };
        if let Err(e) = io::copy(&mut entry, &mut io::sink()) {
            return damaged(format!("{} inside the zip archive is damaged ({e})", entry.name()));
        }
        tested += 1;
    }
    let files = |n: usize| if n == 1 { "1 file".to_owned() } else { format!("{n} files") };
    let mut text = format!("zip archive: {} inside unpacked, no damage found", files(tested));
    if untested > 0 {
        text += &format!(" ({} not tested: packed in a way RDM cannot unpack, or with a password)", files(untested));
    }
    ok(text)
}

async fn video_length(ffmpeg: &Path, path: &Path, expected: f64) -> Option<FileCheck> {
    let text = media::header(ffmpeg, path, &engine::CancellationToken::new()).await.ok()?;
    let Some(length) = length_in(&text) else {
        let unreadable = text.contains("Invalid data found");
        return unreadable.then(|| damaged("ffmpeg cannot read the video".into()));
    };
    Some(if (length - expected).abs() > (expected * 0.01).max(2.0) {
        damaged(format!("the video is {} long, but the site said {}; it may be incomplete", clock(length), clock(expected)))
    } else {
        ok(format!("video: {} long, as the site said", clock(length)))
    })
}

/// The first `Duration: HH:MM:SS.ss` ffmpeg prints, in seconds ("N/A" is no length).
fn length_in(ffmpeg_text: &str) -> Option<f64> {
    let rest = &ffmpeg_text[ffmpeg_text.find("Duration: ")? + "Duration: ".len()..];
    let mut parts = rest.split(',').next()?.trim().split(':');
    let (h, m, s) = (parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?, parts.next()?.parse::<f64>().ok()?);
    Some(h * 3600.0 + m * 60.0 + s)
}

/// 634 -> "10:34", 4000 -> "1:06:40".
fn clock(secs: f64) -> String {
    let s = secs.round() as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// A folder of its own for one test, removed when the guard is dropped.
    fn dir(test: &str) -> (testserver::Folder, PathBuf) {
        let d = testserver::Folder::new(&format!("filecheck-{test}"));
        let path = d.path().to_owned();
        (d, path)
    }

    /// A zip with one deflated and one stored entry; returns its bytes and where the
    /// deflated entry's data starts.
    fn sample_zip(path: &Path) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let text: String = (0..4000).map(|i| format!("line {i} of a text that packs well\n")).collect();
        w.start_file("readme.txt", SimpleFileOptions::default()).unwrap();
        w.write_all(text.as_bytes()).unwrap();
        w.start_file("raw.bin", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
        w.write_all(&[7u8; 5000]).unwrap();
        w.finish().unwrap();
        std::fs::read(path).unwrap()
    }

    #[test]
    fn zip_archives_are_unpacked_and_their_crcs_checked() {
        let (_dir, d) = dir("zip");
        let good = d.join("good.zip");
        let bytes = sample_zip(&good);
        let check = test_zip(&good);
        assert!(check.ok, "{}", check.text);
        assert_eq!(check.text, "zip archive: 2 files inside unpacked, no damage found");

        // One byte changed in the middle of the deflated data (a bad segment): the
        // inflated bytes or their CRC no longer match.
        let mut flipped = bytes.clone();
        flipped[200] ^= 0x55;
        std::fs::write(d.join("flipped.zip"), &flipped).unwrap();
        let check = test_zip(&d.join("flipped.zip"));
        assert!(!check.ok && check.text.contains("readme.txt"), "{}", check.text);

        // A byte of the stored entry: only the CRC can tell.
        let at = bytes.windows(3).position(|w| w == [7, 7, 7]).unwrap() + 100;
        let mut stored = bytes.clone();
        stored[at] = 8;
        std::fs::write(d.join("stored.zip"), &stored).unwrap();
        let check = test_zip(&d.join("stored.zip"));
        assert!(!check.ok && check.text.contains("raw.bin"), "{}", check.text);

        // Cut short: the central directory at the end is gone.
        std::fs::write(d.join("cut.zip"), &bytes[..bytes.len() / 2]).unwrap();
        let check = test_zip(&d.join("cut.zip"));
        assert!(!check.ok && check.text.starts_with("not a readable zip archive"), "{}", check.text);
    }

    #[test]
    fn which_files_are_checked() {
        assert_eq!(kind(Path::new("a/Report.DOCX"), None), Some(Kind::Zip));
        assert_eq!(kind(Path::new("a/app.apk"), None), Some(Kind::Zip));
        assert_eq!(kind(Path::new("a/setup.exe"), None), None);
        assert_eq!(kind(Path::new("a/noext"), None), None);
        assert_eq!(kind(Path::new("a/clip.mp4"), Some(634.5)), Some(Kind::Video { expected: 634.5 }));
        assert_eq!(kind(Path::new("a/clip.mp4"), Some(0.0)), None);
        assert_eq!(kind(Path::new("a/Clip.WEBM"), Some(3.0)), Some(Kind::Video { expected: 3.0 }));
        // Kinds the shipped ffmpeg does not open are left alone, not called damaged.
        for sound in ["a/clip.flv", "a/song.ogg", "a/song.flac", "a/clip.avi", "a/clip"] {
            assert_eq!(kind(Path::new(sound), Some(3.0)), None, "{sound}");
        }
    }

    #[test]
    fn lengths_from_ffmpeg_text() {
        let text = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x.mp4':\n  Duration: 00:10:34.53, start: 0.000000, bitrate: 2 kb/s\n";
        assert_eq!(length_in(text), Some(634.53));
        assert_eq!(length_in("  Duration: 01:06:40.00, start: 0"), Some(4000.0));
        assert_eq!(length_in("  Duration: N/A, start: 0.000000, bitrate: N/A"), None);
        assert_eq!(length_in("x.mp4: Invalid data found when processing input"), None);
        assert_eq!((clock(634.5), clock(59.4), clock(4000.0)), ("10:35".into(), "0:59".into(), "1:06:40".into()));
    }

    /// Needs both ffmpegs (`media::ffmpeg_for_tests`, `media::full_ffmpeg_for_tests`). A 3 s
    /// clip against the right length, one that is 10 s off, and a file that is no video.
    #[tokio::test]
    async fn video_lengths_are_compared() {
        let ffmpeg = media::ffmpeg_for_tests();
        let (_dir, d) = dir("video");
        let clip = d.join("clip.mp4");
        let status = media::command(&media::full_ffmpeg_for_tests())
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=duration=3:size=160x120:rate=25", "-c:v", "mpeg4"])
            .arg(&clip)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let check = video_length(&ffmpeg, &clip, 3.0).await.unwrap();
        assert!(check.ok, "{}", check.text);
        let check = video_length(&ffmpeg, &clip, 13.0).await.unwrap();
        assert_eq!(check.text, "the video is 0:03 long, but the site said 0:13; it may be incomplete");
        let junk = d.join("junk.mp4");
        std::fs::write(&junk, [0u8; 4096]).unwrap();
        let check = video_length(&ffmpeg, &junk, 3.0).await.unwrap();
        assert!(!check.ok, "{}", check.text);

        // Why `kind` passes only what is in FFMPEG_OPENS: every kind in it opens, and a
        // sound file of another kind reads as broken.
        let full = media::full_ffmpeg_for_tests();
        let sample = |name: &str| {
            let (file, mut cmd) = (d.join(name), media::command(&full));
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=3"]);
            // 3GP's own sound takes no 44.1 kHz; AAC is allowed in it too.
            if name.ends_with(".3gp") || name.ends_with(".3g2") {
                cmd.args(["-c:a", "aac"]);
            }
            cmd.arg(&file);
            (file, cmd)
        };
        for ext in FFMPEG_OPENS {
            let (file, mut cmd) = sample(&format!("sound.{ext}"));
            assert!(cmd.status().await.unwrap().success(), "{ext}");
            let check = video_length(&ffmpeg, &file, 3.0).await;
            assert!(check.as_ref().is_some_and(|c| c.ok), "{ext}: {check:?}");
        }
        let (flv, mut cmd) = sample("sound.flv");
        assert!(cmd.status().await.unwrap().success());
        assert!(!video_length(&ffmpeg, &flv, 3.0).await.unwrap().ok, "the shipped ffmpeg opens FLV now: add it to FFMPEG_OPENS");
    }
}
