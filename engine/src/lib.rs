//! Download engine: segmented, resumable HTTP downloads in the style of IDM, and streams
//! made of many pieces (HLS, DASH).

mod checksum;
pub mod dash;
mod download;
mod error;
pub mod hls;
mod limiter;
mod pieces;
mod probe;
mod segments;

use std::time::Duration;

pub use checksum::{Checksum, HashKind, hash_file, parse_hex, to_hex};
pub use download::{Options, Outcome, Progress, download, download_probed, free_path};
pub use error::{Error, Result};
pub use limiter::Limiter;
pub use pieces::{Key, Piece, Pieces, download_pieces};
pub use probe::{Probe, Validator, probe, sanitize as sanitize_file_name};
pub use segments::Segment;
pub use tokio_util::sync::CancellationToken;

pub(crate) const MAX_RETRIES: u32 = 5;

pub(crate) fn client() -> Result<reqwest::Client> {
    Ok(client_builder().build()?)
}

/// For the probe, which follows redirects itself to read every hop's headers.
pub(crate) fn client_without_redirects() -> Result<reqwest::Client> {
    Ok(client_builder().redirect(reqwest::redirect::Policy::none()).build()?)
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("rdm/", env!("CARGO_PKG_VERSION")))
}
