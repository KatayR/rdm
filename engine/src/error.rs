use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;

/// Longest Retry-After the engine honours; longer requests wait this long.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", with_causes(.0))]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("server answered {0}")]
    Status(StatusCode),
    /// 429 or 503: the server wants fewer connections or a pause.
    #[error("server is busy ({status})")]
    Busy { status: StatusCode, retry_after: Option<Duration> },
    #[error("server ignored the requested range: {0}")]
    Range(String),
    /// The remote file is no longer the one being downloaded; what is on disk is useless.
    #[error("the file on the server changed ({0})")]
    Changed(String),
    /// The finished file is not what the server's hash says; it stays a `.part` file.
    #[error("the file does not match the {0} the server gave; Resume downloads it again")]
    Checksum(String),
    #[error("no data for {} s", crate::download::STALL.as_secs())]
    Stalled,
    #[error("connection closed before the segment was complete")]
    ShortBody,
    #[error("download incomplete")]
    Incomplete,
    #[error("stopped")]
    Cancelled,
    #[error("invalid request header {0}")]
    Header(String),
    #[error("streaming playlist: {0}")]
    Playlist(String),
    #[error("this is a live stream, which RDM cannot record yet")]
    Live,
    #[error("the stream is DRM-protected ({0}), which RDM cannot download")]
    Protected(String),
    #[error("a piece could not be decrypted (wrong key)")]
    Decrypt,
    #[error("bad resume state: {0}")]
    State(#[from] serde_json::Error),
}

impl Error {
    /// Error for a response whose status is not the one asked for.
    pub(crate) fn from_response(resp: &reqwest::Response) -> Error {
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            Error::Busy { status, retry_after: retry_after(resp) }
        } else {
            Error::Status(status)
        }
    }

    /// Network trouble worth another attempt; anything else fails the connection at once.
    pub(crate) fn retryable(&self) -> bool {
        match self {
            Error::Http(_) | Error::Stalled | Error::ShortBody | Error::Busy { .. } => true,
            Error::Status(s) => s.is_server_error() || *s == StatusCode::REQUEST_TIMEOUT,
            _ => false,
        }
    }

    /// The server says there is no file at the address (404 Not Found, 410 Gone). Other
    /// refusals (403 and the like) may pass: a quota or a block that is lifted later.
    pub fn is_gone(&self) -> bool {
        matches!(self, Error::Status(s) if *s == StatusCode::NOT_FOUND || *s == StatusCode::GONE)
    }

    /// Wait before attempt number `failures + 1`: exponential backoff from 0.5 s up to 8 s,
    /// or the server's Retry-After when that is longer.
    pub(crate) fn retry_delay(&self, failures: u32) -> Duration {
        let backoff = Duration::from_millis(500 << failures.saturating_sub(1).min(4));
        match self {
            Error::Busy { retry_after: Some(wait), .. } => backoff.max((*wait).min(MAX_RETRY_AFTER)),
            _ => backoff,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Retry-After as seconds or as an HTTP date.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let value = resp.headers().get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse() {
        return Some(Duration::from_secs(secs));
    }
    httpdate::parse_http_date(value).ok()?.duration_since(SystemTime::now()).ok()
}

/// reqwest's message alone ("error sending request") hides the actual cause.
fn with_causes(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        out += &format!(": {c}");
        cause = c.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays() {
        let busy = |s| Error::Busy { status: StatusCode::SERVICE_UNAVAILABLE, retry_after: Some(Duration::from_secs(s)) };
        assert_eq!(Error::Stalled.retry_delay(1), Duration::from_millis(500));
        assert_eq!(Error::Stalled.retry_delay(3), Duration::from_secs(2));
        assert_eq!(Error::Stalled.retry_delay(9), Duration::from_secs(8));
        assert_eq!(busy(5).retry_delay(1), Duration::from_secs(5));
        assert_eq!(busy(0).retry_delay(2), Duration::from_secs(1));
        assert_eq!(busy(3600).retry_delay(1), MAX_RETRY_AFTER);
    }

    #[test]
    fn gone_files() {
        let status = |s| Error::Status(StatusCode::from_u16(s).unwrap());
        assert!(status(404).is_gone() && status(410).is_gone());
        for s in [400, 401, 403, 408, 451, 500] {
            assert!(!status(s).is_gone(), "{s}");
        }
        let busy = Error::Busy { status: StatusCode::TOO_MANY_REQUESTS, retry_after: None };
        assert!(!busy.is_gone() && !Error::Stalled.is_gone());
    }
}
