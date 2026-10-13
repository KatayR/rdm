//! Files on disk: free names (and recognising one), moving and deleting a download's
//! files, and the hash check of a finished file.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use engine::HashKind;

use super::Manager;
use super::file_info::Move;
use super::streams::remove_stream_parts;
use crate::model::{Download, HashCheck, Status};
use crate::{Error, Result};

impl Manager {
    /// The finished file's hash, compared with `expected` (as a site lists it: its length
    /// tells the kind), or its SHA-256 when `expected` is empty.
    pub async fn check_hash(&self, id: i64, expected: &str) -> Result<HashCheck> {
        let wanted = expected_hash(expected)?;
        let d = self.get(id)?;
        let Some(path) = d.path.filter(|_| d.status == Status::Complete) else {
            return Err(Error::Invalid(format!("download {id} is not complete")));
        };
        let kind = wanted.as_ref().map_or(HashKind::Sha256, |w| w.0);
        let hash = tokio::task::spawn_blocking(move || engine::hash_file(&path, kind)).await.map_err(std::io::Error::other)??;
        Ok(HashCheck { kind: kind.name().into(), matches: wanted.map(|(_, value)| value == hash), hash: engine::to_hex(&hash) })
    }
}

/// A removed download's partial files, and with `delete_file` the finished one.
pub(super) fn delete_files(d: &Download, delete_file: bool) -> Result<()> {
    if let (Some(dir), Some(name)) = (&d.dir, &d.file_name) {
        // Only a plain, unfinished download that was given its name has them. One not
        // placed yet only asks for the name, a complete one's part file became the finished
        // file, and a video's streams have names of their own: `<name>.part` is then
        // another download's of the same name.
        if d.placed && d.status != Status::Complete && d.video.is_none() {
            for leftover in [format!("{name}.part"), format!("{name}.part.json")] {
                remove_if_exists(&dir.join(leftover))?;
            }
        }
        // A video's streams, finished or not (the joined file is `path`).
        if d.video.is_some() {
            remove_stream_parts(dir, &stem_of(name), d.id)?;
        }
    }
    if delete_file && let Some(path) = &d.path {
        remove_if_exists(path)?;
    }
    Ok(())
}

/// Moves a stopped download's files to `dir/name`: the finished file (to a free name), or
/// the part file with its resume state. Before the server answered there is nothing yet.
pub(super) fn move_download(d: &mut Download, dir: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    if let (Status::Complete, Some(path)) = (d.status, &d.path) {
        let to = engine::free_path(dir, name);
        move_file(path, &to)?;
        d.file_name = to.file_name().map(|n| n.to_string_lossy().into_owned());
        d.path = Some(to);
    } else {
        if let (Some(old_dir), Some(old_name)) = (&d.dir, &d.file_name) {
            for suffix in [".part", ".part.json"] {
                let (from, to) = (old_dir.join(format!("{old_name}{suffix}")), dir.join(format!("{name}{suffix}")));
                if from.exists() {
                    if to.exists() {
                        return Err(Error::Invalid(format!("{} is in use by another download", to.display())));
                    }
                    move_file(&from, &to)?;
                }
            }
        }
        d.file_name = Some(name.to_owned());
    }
    d.dir = Some(dir.to_owned());
    Ok(())
}

/// Renames, or copies and deletes when `to` is on another drive.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

/// `name`, or `stem (n).ext` when another unfinished download (or a file) in `dir` already
/// uses it, or another download's files are moving to it, so no two downloads ever share a
/// part file. A download uses a name once it was given it (`Download::placed`); one that
/// only asks for it gets its turn here when it starts.
pub(super) fn free_name(
    downloads: &BTreeMap<i64, Download>,
    moving: &HashMap<i64, Move>,
    id: i64,
    dir: &Path,
    name: &str,
) -> String {
    let taken = |n: &str| {
        downloads.values().any(|o| {
            o.id != id
                && o.placed
                && o.status != Status::Complete
                && o.video.is_none()
                && o.dir.as_deref() == Some(dir)
                && o.file_name.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(n))
        }) || moving.iter().any(|(&other, m)| other != id && m.dir == dir && m.name.eq_ignore_ascii_case(n))
    };
    if !taken(name) {
        return name.to_owned();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|n| !taken(n) && !dir.join(n).exists() && !dir.join(format!("{n}.part")).exists())
        .expect("some name is free")
}

/// `name` is `own`, or `own` with the " (n)" that `free_name` gives (any case).
pub(super) fn same_or_numbered(name: &str, own: &str) -> bool {
    if name.eq_ignore_ascii_case(own) {
        return true;
    }
    let split = |n: &str| match n.rfind('.') {
        Some(dot) if dot > 0 => (n[..dot].to_owned(), n[dot..].to_owned()),
        _ => (n.to_owned(), String::new()),
    };
    let ((stem, ext), (own_stem, own_ext)) = (split(name), split(own));
    let Some(number) = stem.strip_prefix(&own_stem).and_then(|r| r.strip_prefix(" (")).and_then(|r| r.strip_suffix(')')) else {
        return false;
    };
    ext.eq_ignore_ascii_case(&own_ext) && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}

/// `name` without its extension.
pub(super) fn stem_of(name: &str) -> String {
    Path::new(name).file_stem().map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned())
}

pub(super) fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// A hash as sites list it, in hex: 32, 40, 64 or 128 digits for MD5, SHA-1, SHA-256 or
/// SHA-512. Anything after the first word is left out ("<hash>  file.iso" from a sums
/// file). None when `text` is empty.
fn expected_hash(text: &str) -> Result<Option<(HashKind, Vec<u8>)>> {
    let Some(word) = text.split_whitespace().next() else { return Ok(None) };
    let value = engine::parse_hex(word).and_then(|v| HashKind::of_len(v.len()).map(|kind| (kind, v)));
    match value {
        Some(found) => Ok(Some(found)),
        None => Err(Error::Invalid("not a hash: expected 32, 40, 64 or 128 hex digits (MD5, SHA-1, SHA-256, SHA-512)".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<name>.part` and its state go with a download only when they can be its own: it
    /// was given the name, is unfinished, and is no video.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_its_own_part_file_goes_with_a_download() {
        use crate::model::NewDownload;
        let (dir, m, _db) = super::super::testing::open("own-part");
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        type Make = fn(&mut Download);
        let cases: [(&str, Make, bool); 4] = [
            ("only asking for the name", |_| {}, false),
            ("complete", |d| (d.placed, d.status) = (true, Status::Complete), false),
            ("a video", |d| (d.placed, d.video) = (true, Some("best".into())), false),
            ("given the name and unfinished", |d| d.placed = true, true),
        ];
        for (what, make, own) in cases {
            let parts = ["same.bin.part", "same.bin.part.json"].map(|name| downloads.join(name));
            for part in &parts {
                std::fs::write(part, what).unwrap();
            }
            let new = NewDownload {
                url: "http://127.0.0.1:9/same.bin".into(),
                dir: Some(downloads.clone()),
                file_name: Some("same.bin".into()),
                ..NewDownload::default()
            };
            let id = m.add(new).unwrap().id;
            make(m.inner.state().downloads.get_mut(&id).unwrap());
            m.remove(id, false).await.unwrap();
            assert_eq!(parts.map(|part| part.exists()), [!own; 2], "{what}");
        }
        m.shutdown().await;
    }

    #[test]
    fn names_a_new_download_may_take() {
        assert!(same_or_numbered("movie.mp4", "movie.mp4"));
        assert!(same_or_numbered("Movie.MP4", "movie.mp4"));
        assert!(same_or_numbered("movie (1).mp4", "movie.mp4"));
        assert!(same_or_numbered("movie (12).mp4", "movie.mp4"));
        assert!(same_or_numbered("archive (2)", "archive"));
        assert!(!same_or_numbered("movie (x).mp4", "movie.mp4"));
        assert!(!same_or_numbered("movie ().mp4", "movie.mp4"));
        assert!(!same_or_numbered("movie (1).mkv", "movie.mp4"));
        assert!(!same_or_numbered("other.mp4", "movie.mp4"));
        assert!(!same_or_numbered("movie 1.mp4", "movie.mp4"));
    }

    #[test]
    fn hashes_as_sites_list_them() {
        let kind = |t: &str| expected_hash(t).unwrap().map(|(k, _)| k);
        assert_eq!(kind(""), None);
        assert_eq!(kind("  \n"), None);
        assert_eq!(kind(&"a".repeat(32)), Some(HashKind::Md5));
        assert_eq!(kind(&"B".repeat(40)), Some(HashKind::Sha1));
        assert_eq!(kind(&format!(" {}  rust-1.90.0.tar.xz\n", "0".repeat(64))), Some(HashKind::Sha256));
        assert_eq!(kind(&"f".repeat(128)), Some(HashKind::Sha512));
        assert_eq!(expected_hash("DEADbeef".repeat(4).as_str()).unwrap().unwrap().1[..4], [0xde, 0xad, 0xbe, 0xef]);
        for bad in ["abc", &"a".repeat(33), &"g".repeat(32), &"a".repeat(48)] {
            assert!(expected_hash(bad).is_err(), "{bad}");
        }
    }
}
