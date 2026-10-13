//! First request to a URL: learns the size, range support, final URL, file name and validators.

use percent_encoding::percent_decode_str;
use reqwest::header::{
    AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_TYPE, COOKIE, ETAG, HeaderMap, HeaderName, HeaderValue,
    LAST_MODIFIED, LOCATION, RANGE,
};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::checksum::{self, Checksum};
use crate::{Error, MAX_RETRIES, Result, client_without_redirects};

/// Redirects followed, as many as reqwest follows by itself.
const MAX_REDIRECTS: usize = 10;

#[derive(Debug, Clone)]
pub struct Probe {
    /// URL after redirects; every segment request goes here.
    pub url: Url,
    pub size: Option<u64>,
    /// True when the server answered a range request with 206 and a known total size.
    pub ranges: bool,
    pub file_name: String,
    pub validator: Validator,
    /// Content-Type without parameters, lower case.
    pub content_type: Option<String>,
    /// The server marked the response as a file to save (Content-Disposition: attachment).
    pub attachment: bool,
    /// A hash of the whole file from the server's headers, to check the result against.
    pub checksum: Option<Checksum>,
    /// Extra request headers to send to `url`.
    pub(crate) headers: HeaderMap,
}

impl Probe {
    /// The address is a web page to look at (HTML or XHTML), not a file to save: a page the
    /// server marks as an attachment is a file.
    pub fn is_web_page(&self) -> bool {
        matches!(self.content_type.as_deref(), Some("text/html" | "application/xhtml+xml")) && !self.attachment
    }
}

/// Identifies a version of the remote file, so a resume never mixes two versions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Validator {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validator {
    fn from_headers(h: &HeaderMap) -> Self {
        let get = |name| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Validator { etag: get(ETAG), last_modified: get(LAST_MODIFIED) }
    }

    /// Same remote file: ETags decide when either side has one, otherwise Last-Modified.
    pub fn same_file(&self, other: &Validator) -> bool {
        if self.etag.is_some() || other.etag.is_some() {
            self.etag == other.etag
        } else {
            self.last_modified == other.last_modified
        }
    }

    /// Value for If-Range, so a changed file comes back as 200 instead of mixing versions.
    /// Weak ETags are not allowed there.
    pub fn if_range(&self) -> Option<&str> {
        match &self.etag {
            Some(e) if !e.starts_with("W/") => Some(e),
            _ => self.last_modified.as_deref(),
        }
    }
}

/// Probes `url`, sending `headers` (referer, cookies, user agent ...) as a browser would,
/// and retrying while the server is busy or the network fails.
pub async fn probe(url: &str, headers: &[(String, String)]) -> Result<Probe> {
    let headers = header_map(headers)?;
    let client = client_without_redirects()?;
    let mut failures = 0;
    loop {
        match probe_once(&client, url, &headers).await {
            Ok(mut info) => {
                let original = Url::parse(url).ok();
                info.headers = headers_for(&info.url, original.as_ref(), headers);
                return Ok(info);
            }
            Err(e) if e.retryable() && failures < MAX_RETRIES => {
                failures += 1;
                sleep(e.retry_delay(failures)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) fn header_map(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let bad = || Error::Header(name.clone());
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| bad())?;
        map.insert(name, HeaderValue::from_str(value).map_err(|_| bad())?);
    }
    Ok(map)
}

/// Headers for requests to `target`. Cookies and credentials belong to the host they came
/// from, so they are dropped when a redirect led to another host (as a browser would).
pub(crate) fn headers_for(target: &Url, original: Option<&Url>, mut headers: HeaderMap) -> HeaderMap {
    if original.and_then(Url::host_str) != target.host_str() {
        headers.remove(COOKIE);
        headers.remove(AUTHORIZATION);
    }
    headers
}

async fn probe_once(client: &Client, url: &str, extra: &HeaderMap) -> Result<Probe> {
    // Redirects are followed here rather than by reqwest, to read every hop's headers:
    // mirror redirectors and Hugging Face give the file's hash only on the redirect.
    let mut resp = client.get(url).headers(extra.clone()).header(RANGE, "bytes=0-0").send().await?;
    let original = resp.url().clone();
    let mut checksums = Vec::new();
    for _ in 0..MAX_REDIRECTS {
        let Some(next) = redirect_target(&resp) else { break };
        checksums.extend(checksum::from_headers(resp.status(), resp.headers()));
        let headers = headers_for(&next, Some(&original), extra.clone());
        resp = client.get(next).headers(headers).header(RANGE, "bytes=0-0").send().await?;
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::from_response(&resp));
    }
    let url = resp.url().clone();
    let headers = resp.headers();
    let validator = Validator::from_headers(headers);
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    let attachment = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("attachment"));
    let file_name = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(disposition_name)
        .or_else(|| url_name(&url))
        .unwrap_or_else(|| "download".to_owned());
    let (size, ranges) = if status == StatusCode::PARTIAL_CONTENT {
        let total = headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range)
            .and_then(|r| r.2);
        (total, total.is_some())
    } else {
        (resp.content_length(), false)
    };
    checksums.extend(checksum::from_headers(status, headers));
    let checksum = checksum::pick(&checksums, headers);
    if status == StatusCode::PARTIAL_CONTENT {
        // Read the single byte so the connection goes back to the pool.
        resp.bytes().await?;
    }
    Ok(Probe { url, size, ranges, file_name, validator, content_type, attachment, checksum, headers: HeaderMap::new() })
}

/// Where a redirect leads, if `resp` is one.
fn redirect_target(resp: &reqwest::Response) -> Option<Url> {
    let redirect = matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308);
    let location = resp.headers().get(LOCATION)?.to_str().ok()?;
    redirect.then(|| resp.url().join(location).ok()).flatten()
}

/// Parses `bytes a-b/total` into `(a, b, total)`; total is None for `*`.
pub(crate) fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let (range, total) = v.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let total = if total == "*" { None } else { Some(total.parse().ok()?) };
    Some((a.parse().ok()?, b.parse().ok()?, total))
}

/// File name from Content-Disposition, preferring the RFC 5987 `filename*` form.
fn disposition_name(v: &str) -> Option<String> {
    let mut plain = None;
    for part in v.split(';') {
        let Some((key, value)) = part.split_once('=') else { continue };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // charset'language'percent-encoded
                if let Some(encoded) = value.splitn(3, '\'').nth(2)
                    && let Some(name) = sanitize(&percent_decode_str(encoded).decode_utf8_lossy())
                {
                    return Some(name);
                }
            }
            "filename" => plain = Some(value.to_owned()),
            _ => {}
        }
    }
    plain.and_then(|p| sanitize(&p))
}

fn url_name(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    sanitize(&percent_decode_str(last).decode_utf8_lossy())
}

/// Makes a server-supplied name safe as a file name on Windows and macOS; anything up to
/// the last slash is dropped.
pub fn sanitize(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']);
    if cleaned.is_empty() {
        return None;
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved { format!("_{cleaned}") } else { cleaned.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition() {
        assert_eq!(disposition_name("attachment; filename=\"a b.zip\"").as_deref(), Some("a b.zip"));
        assert_eq!(disposition_name("attachment; filename=plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(
            disposition_name("attachment; filename=\"fallback.txt\"; filename*=UTF-8''%C3%A7%C4%B1kt%C4%B1.txt").as_deref(),
            Some("çıktı.txt")
        );
        assert_eq!(disposition_name("attachment; filename=\"../../evil.exe\"").as_deref(), Some("evil.exe"));
        assert_eq!(disposition_name("inline"), None);
    }

    #[test]
    fn names_from_urls() {
        let name = |u: &str| url_name(&Url::parse(u).unwrap());
        assert_eq!(name("https://x.org/dir/file%20one.iso?x=1").as_deref(), Some("file one.iso"));
        assert_eq!(name("https://x.org/dir/").as_deref(), Some("dir"));
        assert_eq!(name("https://x.org/"), None);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a:b?.txt").as_deref(), Some("a_b_.txt"));
        assert_eq!(sanitize("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(sanitize("COM1").as_deref(), Some("_COM1"));
        assert_eq!(sanitize("console.txt").as_deref(), Some("console.txt"));
        assert_eq!(sanitize("name. . "), Some("name".to_owned()));
        assert_eq!(sanitize(" .. "), None);
    }

    #[test]
    fn content_ranges() {
        assert_eq!(parse_content_range("bytes 0-0/1234"), Some((0, 0, Some(1234))));
        assert_eq!(parse_content_range("bytes 5-9/*"), Some((5, 9, None)));
        assert_eq!(parse_content_range("bytes */100"), None);
    }

    #[test]
    fn credentials_stay_with_their_host() {
        let pairs = [("Cookie", "a=b"), ("Authorization", "Basic x"), ("Referer", "https://site.org/")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let map = header_map(&pairs).unwrap();
        let url = |u: &str| Url::parse(u).unwrap();
        let same = headers_for(&url("https://a.org/f?x"), Some(&url("https://a.org/get")), map.clone());
        assert_eq!(same.len(), 3);
        let other = headers_for(&url("https://cdn.net/f"), Some(&url("https://a.org/get")), map);
        assert_eq!(other.len(), 1);
        assert!(other.contains_key("referer"));
        assert!(header_map(&[("Bad Name".into(), "x".into())]).is_err());
    }

    #[test]
    fn validators() {
        let v = |e: Option<&str>, l: Option<&str>| Validator { etag: e.map(Into::into), last_modified: l.map(Into::into) };
        assert!(v(Some("\"a\""), Some("x")).same_file(&v(Some("\"a\""), Some("y"))));
        assert!(!v(Some("\"a\""), None).same_file(&v(None, None)));
        assert!(v(None, Some("x")).same_file(&v(None, Some("x"))));
        assert!(!v(None, Some("x")).same_file(&v(None, Some("y"))));
        assert_eq!(v(Some("W/\"a\""), Some("x")).if_range(), Some("x"));
        assert_eq!(v(Some("\"a\""), Some("x")).if_range(), Some("\"a\""));
    }
}
