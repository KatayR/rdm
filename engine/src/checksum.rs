//! Hashes a server gives for the whole file, and checking a file against one.
//!
//! Only headers that describe the whole file are read, since the probe gets a 206 for its
//! first byte: Repr-Digest (RFC 9530) and Digest (RFC 3230, which mirror redirectors such as
//! MirrorBrain put on their redirect), Google Cloud Storage's x-goog-hash, Azure's
//! x-ms-blob-content-md5, Backblaze's X-Bz-Content-Sha1 and Hugging Face's X-Linked-ETag.
//! Content-MD5 and Content-Digest describe the body of their own answer, so they count only
//! on a 200 without Content-Range. S3's checksums and ETags are left out: they can be of
//! one part, or not an MD5 at all.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_ENCODING, CONTENT_RANGE, HeaderMap};
use sha2::Digest;

/// Weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HashKind {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashKind {
    pub fn name(self) -> &'static str {
        match self {
            HashKind::Md5 => "MD5",
            HashKind::Sha1 => "SHA-1",
            HashKind::Sha256 => "SHA-256",
            HashKind::Sha512 => "SHA-512",
        }
    }

    /// Bytes in a hash of this kind.
    pub fn size(self) -> usize {
        match self {
            HashKind::Md5 => 16,
            HashKind::Sha1 => 20,
            HashKind::Sha256 => 32,
            HashKind::Sha512 => 64,
        }
    }

    /// The kind with hashes of `len` bytes.
    pub fn of_len(len: usize) -> Option<HashKind> {
        [HashKind::Md5, HashKind::Sha1, HashKind::Sha256, HashKind::Sha512].into_iter().find(|k| k.size() == len)
    }

    /// Algorithm name as Repr-Digest and Digest write it (any case; "sha" is SHA-1).
    fn from_header_name(name: &str) -> Option<HashKind> {
        match name.trim().to_ascii_lowercase().as_str() {
            "md5" => Some(HashKind::Md5),
            "sha" => Some(HashKind::Sha1),
            "sha-256" => Some(HashKind::Sha256),
            "sha-512" => Some(HashKind::Sha512),
            _ => None,
        }
    }
}

/// A hash of the whole file, as the server gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct Checksum {
    pub kind: HashKind,
    pub value: Vec<u8>,
    /// The header it came from.
    pub header: &'static str,
}

impl Checksum {
    /// "SHA-256 from the server (Repr-Digest)"
    pub fn describe(&self) -> String {
        format!("{} from the server ({})", self.kind.name(), self.header)
    }
}

/// The hashes in one answer, a redirect or the final one.
pub(crate) fn from_headers(status: StatusCode, h: &HeaderMap) -> Vec<Checksum> {
    let mut found = Vec::new();
    let mut add = |header: &'static str, kind: Option<HashKind>, text: &str| {
        if let Some(kind) = kind
            && let Some(value) = decode(kind, text)
        {
            found.push(Checksum { kind, value, header });
        }
    };
    for (key, value) in items(h, "repr-digest").filter_map(|i| i.split_once('=')) {
        add("Repr-Digest", HashKind::from_header_name(key), byte_sequence(value));
    }
    for (key, value) in items(h, "digest").filter_map(|i| i.split_once('=')) {
        add("Digest", HashKind::from_header_name(key), value);
    }
    // Google keeps a gzip-stored object's hash of the gzip bytes, but sends it unpacked.
    let stored = h.get("x-goog-stored-content-encoding").and_then(|v| v.to_str().ok());
    if stored.is_none_or(|e| e.trim().eq_ignore_ascii_case("identity")) {
        for value in items(h, "x-goog-hash").filter_map(|i| i.strip_prefix("md5=")) {
            add("x-goog-hash", Some(HashKind::Md5), value);
        }
    }
    for value in items(h, "x-ms-blob-content-md5") {
        add("x-ms-blob-content-md5", Some(HashKind::Md5), value);
    }
    // "none" for large files; "unverified:" when only the uploader vouched for it.
    for value in items(h, "x-bz-content-sha1").filter(|v| !v.contains(':')) {
        add("X-Bz-Content-Sha1", Some(HashKind::Sha1), value);
    }
    // The SHA-256 of a file kept in Git LFS or Xet, on the redirect to it.
    for value in items(h, "x-linked-etag").map(|v| v.trim_matches('"')).filter(|v| v.len() == 64) {
        add("X-Linked-ETag", Some(HashKind::Sha256), value);
    }
    if status == StatusCode::OK && !h.contains_key(CONTENT_RANGE) {
        for value in items(h, "content-md5") {
            add("Content-MD5", Some(HashKind::Md5), value);
        }
        for (key, value) in items(h, "content-digest").filter_map(|i| i.split_once('=')) {
            add("Content-Digest", HashKind::from_header_name(key), byte_sequence(value));
        }
    }
    found
}

/// The hash to check the file against: the strongest kind whose sources agree. None when
/// the final answer is content-coded (the hashes may then be of other bytes than saved).
pub(crate) fn pick(found: &[Checksum], last: &HeaderMap) -> Option<Checksum> {
    let coded = last.get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|e| !e.trim().eq_ignore_ascii_case("identity"));
    if coded {
        return None;
    }
    let mut kinds: Vec<HashKind> = found.iter().map(|c| c.kind).collect();
    kinds.sort_unstable_by(|a, b| b.cmp(a));
    kinds.dedup();
    kinds.into_iter().find_map(|kind| {
        let mut of_kind = found.iter().filter(|c| c.kind == kind);
        let first = of_kind.next()?;
        of_kind.all(|c| c.value == first.value).then(|| first.clone())
    })
}

/// Every comma-separated item of every `name` header, trimmed, without parameters.
fn items<'a>(h: &'a HeaderMap, name: &'static str) -> impl Iterator<Item = &'a str> {
    h.get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|i| i.split(';').next().unwrap_or("").trim())
        .filter(|i| !i.is_empty())
}

/// The inside of a structured-field byte sequence (`:base64:`).
fn byte_sequence(v: &str) -> &str {
    v.trim().strip_prefix(':').and_then(|v| v.strip_suffix(':')).unwrap_or("")
}

/// Base64 (the standard form), or hex for the headers that use it; None unless the result
/// has the kind's length, so a value in an unexpected form is ignored, never a mismatch.
fn decode(kind: HashKind, text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    let hex = (text.len() == 2 * kind.size()).then(|| parse_hex(text)).flatten();
    let value = hex.or_else(|| STANDARD.decode(text).ok())?;
    (value.len() == kind.size()).then_some(value)
}

pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash of the file at `path`; reads it once, in large blocks.
pub fn hash_file(path: &Path, kind: HashKind) -> io::Result<Vec<u8>> {
    fn run<D: Digest>(mut file: File) -> io::Result<Vec<u8>> {
        let mut hasher = D::new();
        let mut buf = vec![0; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(hasher.finalize().to_vec());
            }
            hasher.update(&buf[..n]);
        }
    }
    let file = File::open(path)?;
    match kind {
        HashKind::Md5 => run::<md5::Md5>(file),
        HashKind::Sha1 => run::<sha1::Sha1>(file),
        HashKind::Sha256 => run::<sha2::Sha256>(file),
        HashKind::Sha512 => run::<sha2::Sha512>(file),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    // SHA-256 and MD5 of "hello".
    const SHA256_HEX: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const SHA256_B64: &str = "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
    const MD5_B64: &str = "XUFAKrxLKna5cZ2REBfFkg==";

    fn sha256() -> Vec<u8> {
        parse_hex(SHA256_HEX).unwrap()
    }

    #[test]
    fn repr_digest_and_digest() {
        let h = headers(&[("repr-digest", &format!("sha-512=:AAAA:, sha-256=:{SHA256_B64}:"))]);
        // The sha-512 value is too short: ignored, not a mismatch.
        assert_eq!(from_headers(StatusCode::PARTIAL_CONTENT, &h), [Checksum { kind: HashKind::Sha256, value: sha256(), header: "Repr-Digest" }]);
        let h = headers(&[("digest", &format!("MD5={MD5_B64},SHA-256={SHA256_B64}"))]);
        let found = from_headers(StatusCode::FOUND, &h);
        assert_eq!(found.len(), 2);
        assert_eq!(pick(&found, &HeaderMap::new()).unwrap().kind, HashKind::Sha256);
        // RFC 3230 algorithm names are not case-sensitive; "SHA" is SHA-1.
        let h = headers(&[("digest", "sha=qvTGHdzF6KLavt4PO0gs2a6pQ00=")]);
        assert_eq!(from_headers(StatusCode::OK, &h)[0].kind, HashKind::Sha1);
    }

    #[test]
    fn storage_service_headers() {
        let h = headers(&[("x-goog-hash", "crc32c=FnKFFQ=="), ("x-goog-hash", &format!("md5={MD5_B64}"))]);
        assert_eq!(from_headers(StatusCode::PARTIAL_CONTENT, &h)[0].header, "x-goog-hash");
        let gzip = headers(&[("x-goog-hash", &format!("crc32c=FnKFFQ==,md5={MD5_B64}")), ("x-goog-stored-content-encoding", "gzip")]);
        assert!(from_headers(StatusCode::PARTIAL_CONTENT, &gzip).is_empty());
        let h = headers(&[("x-linked-etag", &format!("\"{SHA256_HEX}\""))]);
        assert_eq!(from_headers(StatusCode::FOUND, &h)[0].value, sha256());
        let h = headers(&[("x-bz-content-sha1", "unverified:aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")]);
        assert!(from_headers(StatusCode::OK, &h).is_empty());
        let h = headers(&[("x-bz-content-sha1", "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")]);
        assert_eq!(from_headers(StatusCode::OK, &h)[0].kind, HashKind::Sha1);
        assert!(from_headers(StatusCode::OK, &headers(&[("x-bz-content-sha1", "none")])).is_empty());
    }

    #[test]
    fn body_hashes_only_for_a_whole_body() {
        let h = headers(&[("content-md5", MD5_B64)]);
        assert_eq!(from_headers(StatusCode::OK, &h).len(), 1);
        assert!(from_headers(StatusCode::PARTIAL_CONTENT, &h).is_empty());
        let ranged = headers(&[("content-md5", MD5_B64), ("content-range", "bytes 0-4/5")]);
        assert!(from_headers(StatusCode::OK, &ranged).is_empty());
    }

    #[test]
    fn picking() {
        let c = |kind, byte: u8| Checksum { kind, value: vec![byte; kind.size()], header: "Digest" };
        // Two sources disagree on SHA-256: the MD5 they agree on is used.
        let found = [c(HashKind::Sha256, 1), c(HashKind::Sha256, 2), c(HashKind::Md5, 3), c(HashKind::Md5, 3)];
        assert_eq!(pick(&found, &HeaderMap::new()), Some(c(HashKind::Md5, 3)));
        assert_eq!(pick(&found[..2], &HeaderMap::new()), None);
        assert_eq!(pick(&found[2..], &headers(&[("content-encoding", "gzip")])), None);
        assert_eq!(pick(&found[2..], &headers(&[("content-encoding", "identity")])), Some(c(HashKind::Md5, 3)));
    }

    #[test]
    fn file_hashes() {
        let dir = testserver::Folder::new("checksum");
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(hash_file(&path, HashKind::Sha256).unwrap(), sha256());
        assert_eq!(hash_file(&path, HashKind::Md5).unwrap(), STANDARD.decode(MD5_B64).unwrap());
        assert_eq!(to_hex(&hash_file(&path, HashKind::Sha1).unwrap()), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        assert_eq!(to_hex(&hash_file(&path, HashKind::Sha512).unwrap())[..16], *"9b71d224bd62f378");
    }
}
