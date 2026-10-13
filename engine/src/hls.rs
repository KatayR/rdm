//! HLS playlists (RFC 8216): a media playlist's pieces, in order, and a master playlist's
//! qualities and audio tracks.
//!
//! A playlist is fetched here (`load`) or comes as the text the browser's page already read
//! (`read`, `master`), which also works where the address cannot be asked a second time.

use reqwest::Url;
use reqwest::header::HeaderMap;
use tokio::time::sleep;

use crate::pieces::{Key, Piece, Pieces};
use crate::probe::header_map;
use crate::{Error, MAX_RETRIES, Result, client};

/// Fetches the media playlist at `url` with `headers` and lists its pieces.
pub async fn load(url: &str, headers: &[(String, String)]) -> Result<Pieces> {
    let headers = header_map(headers)?;
    let client = client()?;
    let mut failures = 0;
    loop {
        match fetch(&client, url, &headers).await {
            Ok((text, base)) => return parse(&text, &base, headers),
            Err(e) if e.retryable() && failures < MAX_RETRIES => {
                failures += 1;
                sleep(e.retry_delay(failures)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// The pieces of the media playlist `text`, which was read from `url` elsewhere.
pub fn read(text: &str, url: &str, headers: &[(String, String)]) -> Result<Pieces> {
    parse(text, &address(url)?, header_map(headers)?)
}

fn address(url: &str) -> Result<Url> {
    Url::parse(url).map_err(|_| Error::Playlist(format!("bad playlist address {url}")))
}

/// True for a master playlist: it lists qualities, not pieces.
pub fn is_master(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with("#EXT-X-STREAM-INF"))
}

/// One quality of a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    /// Its media playlist.
    pub url: Url,
    /// Bits per second: the average when the playlist gives it, else the peak.
    pub bandwidth: Option<u64>,
    /// Width and height.
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    /// As the playlist lists them: `avc1.64002a`, `mp4a.40.2`.
    pub codecs: Vec<String>,
    /// The group of audio tracks that carries its sound, when that is separate.
    pub audio: Option<String>,
    /// `SDR`, `PQ` or `HLG`.
    pub video_range: Option<String>,
}

/// One audio track of a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    pub group: String,
    pub name: String,
    pub language: Option<String>,
    /// The track players start with.
    pub default: bool,
    /// Its media playlist; None when the sound is inside the qualities that name the group.
    pub url: Option<Url>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<AudioTrack>,
}

/// The qualities and audio tracks of the master playlist `text`, which came from `url`.
/// Trick-play variants, subtitles and alternative pictures are left out. A master that
/// names a DRM key for all its streams is refused as protected.
pub fn master(text: &str, url: &str) -> Result<Master> {
    let base = address(url)?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next().map(|l| l.trim_start_matches('\u{feff}')) != Some("#EXTM3U") {
        return Err(Error::Playlist("not an HLS playlist".into()));
    }
    let mut master = Master::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(attributes(value));
        } else if let Some(value) = line.strip_prefix("#EXT-X-SESSION-KEY:") {
            if let Err(protected @ Error::Protected(_)) = key_tag(&attributes(value), &base) {
                return Err(protected);
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(value);
            if attr(&attrs, "TYPE") != Some("AUDIO") {
                continue;
            }
            let url = match attr(&attrs, "URI") {
                Some(uri) => Some(base.join(uri).map_err(|_| Error::Playlist(format!("bad audio track address {uri}")))?),
                None => None,
            };
            master.audio.push(AudioTrack {
                group: attr(&attrs, "GROUP-ID").unwrap_or_default().to_owned(),
                name: attr(&attrs, "NAME").unwrap_or_default().to_owned(),
                language: attr(&attrs, "LANGUAGE").map(str::to_owned),
                default: attr(&attrs, "DEFAULT") == Some("YES"),
                url,
            });
        } else if !line.starts_with('#')
            && let Some(attrs) = pending.take()
        {
            let number = |name: &str| attr(&attrs, name).and_then(|v| v.trim().parse::<f64>().ok());
            let resolution = attr(&attrs, "RESOLUTION").and_then(|r| {
                let (w, h) = r.split_once(['x', 'X'])?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            });
            master.variants.push(Variant {
                url: base.join(line).map_err(|_| Error::Playlist(format!("bad quality address {line}")))?,
                bandwidth: number("AVERAGE-BANDWIDTH").or_else(|| number("BANDWIDTH")).map(|b| b as u64),
                resolution,
                frame_rate: number("FRAME-RATE"),
                codecs: attr(&attrs, "CODECS").map(|c| c.split(',').map(|c| c.trim().to_owned()).collect()).unwrap_or_default(),
                audio: attr(&attrs, "AUDIO").map(str::to_owned),
                video_range: attr(&attrs, "VIDEO-RANGE").map(str::to_owned),
            });
        }
    }
    if master.variants.is_empty() {
        return Err(Error::Playlist("the master playlist lists no quality".into()));
    }
    Ok(master)
}

/// Every address the playlist `text` (from `url`) names: a master's qualities and tracks, a
/// media playlist's pieces, initialisation sections and keys. Addresses that cannot be
/// read are left out.
pub fn named(text: &str, url: &str) -> Vec<Url> {
    let Ok(base) = Url::parse(url) else { return Vec::new() };
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match line.split_once(':') {
            Some((tag, value)) if tag.starts_with('#') => {
                out.extend(attributes(value).iter().filter(|(name, _)| name == "URI").filter_map(|(_, uri)| base.join(uri).ok()));
            }
            _ if line.starts_with('#') => {}
            _ => out.extend(base.join(line).ok()),
        }
    }
    out
}

async fn fetch(client: &reqwest::Client, url: &str, headers: &HeaderMap) -> Result<(String, Url)> {
    let resp = client.get(url).headers(headers.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(Error::from_response(&resp));
    }
    let base = resp.url().clone();
    Ok((resp.text().await?, base))
}

/// Pieces of the media playlist `text`, with relative addresses resolved against `base`.
pub fn parse(text: &str, base: &Url, headers: HeaderMap) -> Result<Pieces> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next().map(|l| l.trim_start_matches('\u{feff}')) != Some("#EXTM3U") {
        return Err(Error::Playlist("not an HLS playlist".into()));
    }
    let bad = |what: &str, line: &str| Error::Playlist(format!("bad {what}: {line}"));
    let mut sequence: u64 = 0;
    let mut key: Option<KeyTag> = None;
    let mut init = None;
    let mut pieces = Vec::new();
    let mut duration = 0.0;
    let mut range: Option<(u64, Option<u64>)> = None;
    // A byte range without an offset starts where the previous one of the same file ended.
    let mut last_end: Option<(Url, u64)> = None;
    let (mut ended, mut vod) = (false, false);

    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = value.parse().map_err(|_| bad("media sequence", line))?;
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let secs = value.split(',').next().unwrap_or("");
            duration = secs.trim().parse().map_err(|_| bad("duration", line))?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(byte_range(value).ok_or_else(|| bad("byte range", line))?);
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:") {
            key = key_tag(&attributes(value), base)?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(value);
            let uri = attr(&attrs, "URI").ok_or_else(|| bad("map", line))?;
            let url = base.join(uri).map_err(|_| bad("map address", line))?;
            let range = match attr(&attrs, "BYTERANGE") {
                Some(r) => match byte_range(r) {
                    Some((len, Some(start))) => Some((start, len)),
                    _ => return Err(bad("map byte range", line)),
                },
                None => None,
            };
            if let Some((start, len)) = range {
                last_end = Some((url.clone(), start + len));
            }
            // The initialisation section can only be decrypted with an explicit IV.
            let key = key.as_ref().and_then(|k| k.iv.map(|iv| Key { url: k.url.clone(), iv }));
            init = Some(Piece { url, range, key, duration: 0.0 });
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if let Some(kind) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            vod = kind.trim() == "VOD";
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            return Err(Error::Playlist("a master playlist, not one stream".into()));
        } else if line.starts_with('#') {
            // Comments and tags that do not change what is downloaded.
        } else {
            let url = base.join(line).map_err(|_| bad("piece address", line))?;
            let range = match range.take() {
                Some((len, Some(start))) => Some((start, len)),
                Some((len, None)) => match &last_end {
                    Some((prev, end)) if *prev == url => Some((*end, len)),
                    _ => return Err(bad("byte range without offset", line)),
                },
                None => None,
            };
            if let Some((start, len)) = range {
                last_end = Some((url.clone(), start + len));
            }
            let key = key
                .as_ref()
                .map(|k| Key { url: k.url.clone(), iv: k.iv.unwrap_or_else(|| (sequence as u128).to_be_bytes()) });
            pieces.push(Piece { url, range, key, duration });
            sequence += 1;
            duration = 0.0;
        }
    }
    if !ended && !vod {
        return Err(Error::Live);
    }
    if pieces.is_empty() {
        return Err(Error::Playlist("the playlist has no pieces".into()));
    }
    let ts = init.is_none();
    Ok(Pieces { source: base.clone(), headers, init, pieces, ts, size_hint: None, open_end: false })
}

struct KeyTag {
    url: Url,
    iv: Option<[u8; 16]>,
}

/// The key in force from an EXT-X-KEY tag on; None for METHOD=NONE.
fn key_tag(attrs: &[(String, String)], base: &Url) -> Result<Option<KeyTag>> {
    let method = attr(attrs, "METHOD").unwrap_or("NONE");
    let format = attr(attrs, "KEYFORMAT").unwrap_or("identity");
    match method {
        "NONE" => return Ok(None),
        "AES-128" if format == "identity" => {}
        _ => return Err(Error::Protected(format!("{method}, {format}"))),
    }
    let uri = attr(attrs, "URI").ok_or_else(|| Error::Playlist("AES-128 key without an address".into()))?;
    let url = base.join(uri).map_err(|_| Error::Playlist(format!("bad key address {uri}")))?;
    let iv = match attr(attrs, "IV") {
        Some(hex) => Some(parse_iv(hex).ok_or_else(|| Error::Playlist(format!("bad IV {hex}")))?),
        None => None,
    };
    Ok(Some(KeyTag { url, iv }))
}

/// `0x` and 32 hex digits.
fn parse_iv(hex: &str) -> Option<[u8; 16]> {
    let digits = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    if digits.len() != 32 {
        return None;
    }
    u128::from_str_radix(digits, 16).ok().map(u128::to_be_bytes)
}

/// `<length>[@<offset>]`.
fn byte_range(value: &str) -> Option<(u64, Option<u64>)> {
    let value = value.trim();
    match value.split_once('@') {
        Some((len, start)) => Some((len.parse().ok()?, Some(start.parse().ok()?))),
        None => Some((value.parse().ok()?, None)),
    }
}

/// `NAME=value,NAME="quoted, value"` as pairs, quotes removed.
fn attributes(list: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while let Some((name, after)) = rest.split_once('=') {
        let (value, next) = match after.strip_prefix('"') {
            Some(quoted) => match quoted.split_once('"') {
                Some((v, n)) => (v, n),
                None => (quoted, ""),
            },
            None => after.split_once(',').map_or((after, ""), |(v, n)| (v, n)),
        };
        out.push((name.trim().to_owned(), value.to_owned()));
        rest = next.trim_start_matches(',').trim_start();
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/video/720p/index.m3u8?token=t").unwrap()
    }

    fn pieces(text: &str) -> Result<Pieces> {
        parse(text, &base(), HeaderMap::new())
    }

    #[test]
    fn transport_stream_playlist() {
        let p = pieces(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXTINF:10.0,\nseg7.ts\n#EXTINF:9.5,title\n/abs/seg8.ts?x=1\n#EXTINF:4,\nhttps://other.example/seg9.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        let urls: Vec<&str> = p.pieces.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.example/video/720p/seg7.ts",
                "https://cdn.example/abs/seg8.ts?x=1",
                "https://other.example/seg9.ts"
            ]
        );
        assert_eq!(p.pieces.iter().map(|p| p.duration).sum::<f64>(), 23.5);
        assert!(p.ts && p.init.is_none() && p.pieces.iter().all(|p| p.key.is_none() && p.range.is_none()));
    }

    #[test]
    fn fmp4_with_byte_ranges() {
        let p = pieces(
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"719@0\"\n\
             #EXTINF:6.0,\t\n#EXT-X-BYTERANGE:1508000@719\nmain.mp4\n#EXTINF:6.0,\n#EXT-X-BYTERANGE:1510244\nmain.mp4\n",
        )
        .unwrap();
        let init = p.init.unwrap();
        assert_eq!((init.url.as_str(), init.range), ("https://cdn.example/video/720p/main.mp4", Some((0, 719))));
        assert_eq!(p.pieces[0].range, Some((719, 1508000)));
        assert_eq!(p.pieces[1].range, Some((1508719, 1510244)));
        assert!(!p.ts);
    }

    #[test]
    fn aes_keys_and_ivs() {
        let p = pieces(
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:5\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?a=1,b=2\"\n#EXTINF:1,\na.ts\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"/k2\",IV=0x000102030405060708090A0B0C0D0E0F\n#EXTINF:1,\nb.ts\n\
             #EXT-X-KEY:METHOD=NONE\n#EXTINF:1,\nc.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        let a = p.pieces[0].key.as_ref().unwrap();
        assert_eq!(a.url.as_str(), "https://cdn.example/video/720p/key.bin?a=1,b=2");
        assert_eq!(a.iv, 5u128.to_be_bytes(), "IV from the media sequence number");
        let b = p.pieces[1].key.as_ref().unwrap();
        assert_eq!((b.url.as_str(), b.iv[15]), ("https://cdn.example/k2", 0x0f));
        assert!(p.pieces[2].key.is_none());
    }

    /// Shaped like Apple's example: sound in separate tracks, two groups of them.
    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud1\",LANGUAGE=\"en\",NAME=\"English, low\",AUTOSELECT=YES,DEFAULT=YES,CHANNELS=\"2\",URI=\"a1/prog_index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud2\",NAME=\"English\",URI=\"/abs/a2.m3u8?t=1\"\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub1\",NAME=\"English\",URI=\"s1/en/prog_index.m3u8\"\n\
        #EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=2168183,BANDWIDTH=2177116,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=960x540,FRAME-RATE=60.000,AUDIO=\"aud1\",SUBTITLES=\"sub1\"\n\
        # a comment between the tag and its address\n\
        v5/prog_index.m3u8\n\
        #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=187492,CODECS=\"avc1.640020\",RESOLUTION=960x540,URI=\"v5/iframe_index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=8190919,VIDEO-RANGE=PQ,RESOLUTION=1920X1080,AUDIO=\"aud2\"\n\
        https://other.example/v9.m3u8\n\
        #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=64000\naudio-only.m3u8\n";

    #[test]
    fn master_playlist_qualities_and_audio_tracks() {
        // As a server may send it: a byte order mark first, CRLF line ends.
        let sent = format!("\u{feff}{}", MASTER.replace('\n', "\r\n"));
        assert_eq!(master(&sent, base().as_str()).unwrap(), master(MASTER, base().as_str()).unwrap());
        let m = master(MASTER, base().as_str()).unwrap();
        let v = &m.variants[0];
        assert_eq!(v.url.as_str(), "https://cdn.example/video/720p/v5/prog_index.m3u8");
        assert_eq!((v.bandwidth, v.resolution, v.frame_rate), (Some(2168183), Some((960, 540)), Some(60.0)), "the average, not the peak");
        assert_eq!((v.codecs.as_slice(), v.audio.as_deref()), (&["avc1.640020".to_owned(), "mp4a.40.2".to_owned()][..], Some("aud1")));
        let v = &m.variants[1];
        assert_eq!((v.url.as_str(), v.bandwidth, v.resolution), ("https://other.example/v9.m3u8", Some(8190919), Some((1920, 1080))));
        assert_eq!((v.video_range.as_deref(), v.codecs.len(), v.frame_rate), (Some("PQ"), 0, None));
        assert_eq!((m.variants[2].bandwidth, m.variants[2].resolution, m.variants.len()), (Some(64000), None, 3), "no trick-play variant");
        let track = |group: &str, name: &str, language: Option<&str>, default, url: &str| AudioTrack {
            group: group.into(),
            name: name.into(),
            language: language.map(str::to_owned),
            default,
            url: Some(Url::parse(url).unwrap()),
        };
        assert_eq!(
            m.audio,
            [
                track("aud1", "English, low", Some("en"), true, "https://cdn.example/video/720p/a1/prog_index.m3u8"),
                track("aud2", "English", None, false, "https://cdn.example/abs/a2.m3u8?t=1"),
            ],
            "subtitles are no audio track"
        );
        assert!(is_master(MASTER) && !is_master("#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n"));
        let err = |t: &str| master(t, base().as_str()).unwrap_err().to_string();
        assert!(err("#EXTM3U\n#EXTINF:1,\na.ts\n").contains("no quality"));
        let fairplay = "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n";
        assert!(matches!(master(fairplay, base().as_str()), Err(Error::Protected(_))));
        let plain_key = "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"k\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n";
        assert_eq!(master(plain_key, base().as_str()).unwrap().variants.len(), 1, "AES-128 is no DRM");
        assert!(err("<html>").contains("not an HLS playlist"));
    }

    /// A playlist the page built has no address to resolve against: full addresses still read.
    #[test]
    fn playlists_under_a_blob_address() {
        let blob = "blob:https://site.example/0b1c-2d3e";
        let m = master("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://cdn.example/v.m3u8\n", blob).unwrap();
        assert_eq!(m.variants[0].url.as_str(), "https://cdn.example/v.m3u8");
        assert!(master("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n", blob).is_err());
        let p = read("#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/s0.ts\n#EXT-X-ENDLIST\n", blob, &[]).unwrap();
        assert_eq!((p.pieces[0].url.as_str(), p.duration()), ("https://cdn.example/s0.ts", 4.0));
    }

    #[test]
    fn addresses_a_playlist_names() {
        let urls = |text: &str| named(text, base().as_str()).iter().map(|u| u.to_string()).collect::<Vec<_>>();
        assert_eq!(
            urls(MASTER),
            [
                "https://cdn.example/video/720p/a1/prog_index.m3u8",
                "https://cdn.example/abs/a2.m3u8?t=1",
                "https://cdn.example/video/720p/s1/en/prog_index.m3u8",
                "https://cdn.example/video/720p/v5/prog_index.m3u8",
                "https://cdn.example/video/720p/v5/iframe_index.m3u8",
                "https://other.example/v9.m3u8",
                "https://cdn.example/video/720p/audio-only.m3u8",
            ]
        );
        let media = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"719@0\"\n\
            #EXTINF:6.0,a title, with URI=\"no\"\n#EXT-X-BYTERANGE:10@719\nmain.mp4\n#EXT-X-ENDLIST\n";
        assert_eq!(
            urls(media),
            ["https://cdn.example/video/720p/key.bin", "https://cdn.example/video/720p/main.mp4", "https://cdn.example/video/720p/main.mp4"]
        );
        assert!(named(media, "not an address").is_empty());
    }

    #[test]
    fn refusals() {
        let err = |t: &str| pieces(t).unwrap_err().to_string();
        assert!(matches!(pieces("#EXTM3U\n#EXTINF:1,\na.ts\n"), Err(Error::Live)));
        assert!(err("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n").contains("master"));
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(matches!(pieces(drm), Err(Error::Protected(_))));
        let other_format = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"com.widevine\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(matches!(pieces(other_format), Err(Error::Protected(_))));
        assert!(err("<html>").contains("not an HLS playlist"));
        assert!(err("#EXTM3U\n#EXT-X-ENDLIST\n").contains("no pieces"));
    }
}
