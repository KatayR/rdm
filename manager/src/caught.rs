//! What the browser's tab played, sorted out for the Download Video window, and HLS and
//! DASH playlists listed from the text the page itself read.
//!
//! The extension reports every playlist and media file a tab received, and for a playlist
//! the page's script read, its text. Here the list is tidied (a quality's playlist belongs
//! to its master, a playlist's pieces are no files of their own) and a playlist with text
//! is listed without asking its address again: some answer once, some only to a POST, some
//! only to the page, and one the page built has no address at all. The listing is written
//! as yt-dlp would write it, so the quality list and the choice of streams are the same
//! code as for a page (`video`).

use std::collections::{BTreeSet, HashSet};

use engine::{dash, hls};
use reqwest::Url;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::model::{Caught, CaughtKind, Playlist};
use crate::video::{self, Resolved, VideoInfo};
use crate::{Error, Result};

/// One thing to offer from what a tab played.
#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// Its place in `VideoPage::caught`; a pick names it.
    pub index: usize,
    pub url: String,
    pub kind: CaughtKind,
    pub size: Option<u64>,
    pub headers: Vec<(String, String)>,
    pub earlier: bool,
    /// The qualities of a playlist listed from its text. None for a plain file, and for a
    /// playlist yt-dlp has to be asked about by its address.
    pub info: Option<VideoInfo>,
}

/// What to offer from `caught`, in its order. Left out: what another caught playlist's text
/// names (a master's qualities and audio tracks, a playlist's pieces), a playlist that names
/// the same addresses as one offered before it (one video, seen twice), and a playlist
/// without an address that cannot be listed.
pub fn sources(caught: &[Caught]) -> Vec<Source> {
    let named: Vec<BTreeSet<String>> = caught.iter().map(names).collect();
    let mut offered = HashSet::new();
    let mut out = Vec::new();
    for (index, c) in caught.iter().enumerate() {
        let address = same(&c.url);
        if named.iter().enumerate().any(|(other, names)| other != index && names.contains(&address)) {
            continue;
        }
        // The same text under another address may be another video (its addresses are
        // resolved against its own), so what counts is what is named.
        let names = Some(&named[index]).filter(|names| !names.is_empty());
        if names.is_some_and(|names| offered.contains(names)) {
            continue;
        }
        let info = listed(caught, index).map(|(_, info)| info);
        if info.is_none() && !web(&c.url) {
            continue;
        }
        offered.extend(names);
        out.push(Source { index, url: c.url.clone(), kind: c.kind, size: c.size, headers: c.headers.clone(), earlier: c.earlier, info });
    }
    out
}

/// The playlists to keep with a download picked from `caught[index]`: that playlist, then
/// the ones it names whose text is known. Empty when it has no text or cannot be listed
/// from it; the download then goes by its address (yt-dlp).
pub fn saved(caught: &[Caught], index: usize) -> Vec<Playlist> {
    listed(caught, index).map(|(playlists, _)| playlists).unwrap_or_default()
}

fn listed(caught: &[Caught], index: usize) -> Option<(Vec<Playlist>, VideoInfo)> {
    let root = caught.get(index)?;
    let text = playlist_text(root)?;
    let named = names(root);
    let mut playlists = vec![Playlist { url: root.url.clone(), text: text.to_owned() }];
    for (i, c) in caught.iter().enumerate() {
        if i != index
            && named.contains(&same(&c.url))
            && let Some(text) = playlist_text(c)
        {
            playlists.push(Playlist { url: c.url.clone(), text: text.to_owned() });
        }
    }
    let played: HashSet<String> = caught.iter().map(|c| same(&c.url)).collect();
    let info = video::video_info(&listing(&playlists, &root.headers, &played).ok()?).ok()?;
    Some((playlists, info))
}

/// The streams of `format` (from `sources`' quality list) of a download's saved
/// `playlists`, asked for with `headers`.
pub fn resolve(playlists: &[Playlist], headers: &[(String, String)], format: &str) -> Result<Resolved> {
    video::from_listing(&listing(playlists, headers, &HashSet::new())?, format)?
        .ok_or_else(|| Error::Tool(format!("the saved playlist has no quality {format}")))
}

/// The saved text of the playlist at `url`, if the page read that one.
pub fn text_of<'a>(playlists: &'a [Playlist], url: &str) -> Option<&'a str> {
    let address = same(url);
    playlists.iter().find(|p| same(&p.url) == address).map(|p| p.text.as_str())
}

/// The text of a caught playlist, when the page read it.
fn playlist_text(c: &Caught) -> Option<&str> {
    c.text.as_deref().filter(|_| matches!(c.kind, CaughtKind::Hls | CaughtKind::Dash))
}

/// Every address a caught playlist's text names, each written one way.
fn names(c: &Caught) -> BTreeSet<String> {
    let urls = match playlist_text(c) {
        Some(text) if dash::is_manifest(text) => dash::named(text, &c.url),
        Some(text) => hls::named(text, &c.url),
        None => Vec::new(),
    };
    urls.iter().map(|u| same(u.as_str())).collect()
}

fn web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// An address as written the same way everywhere, without its fragment.
fn same(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.into()
        }
        Err(_) => url.to_owned(),
    }
}

/// What `playlists[0]` offers, as yt-dlp lists a playlist address (`video::video_info` and
/// `video::from_listing` read it): an HLS master's qualities and audio tracks, the one
/// stream of a media playlist, or a DASH manifest's streams. The other playlists give an
/// HLS video's length. `played`: the addresses the tab loaded, when known; they tell which
/// of a DASH manifest's languages was listened to.
fn listing(playlists: &[Playlist], headers: &[(String, String)], played: &HashSet<String>) -> Result<Vec<u8>> {
    let root = playlists.first().ok_or_else(|| Error::Tool("no playlist was saved".into()))?;
    let host = Url::parse(&root.url).ok().and_then(|u| u.host_str().map(str::to_owned));
    // Cookies stay with the host the browser sent them to (a playlist the page built has
    // none).
    let headers_for = |url: &Url| -> Value {
        let own = host.is_some() && url.host_str().map(str::to_owned) == host;
        let kept = headers.iter().filter(|(name, _)| own || !(name.eq_ignore_ascii_case("cookie") || name.eq_ignore_ascii_case("authorization")));
        Value::Object(kept.map(|(name, value)| (name.clone(), json!(value))).collect::<Map<_, _>>())
    };
    let mut formats = Vec::new();
    let duration;
    if dash::is_manifest(&root.text) {
        let manifest = dash::read(&root.text, &root.url)?;
        duration = manifest.duration.unwrap_or(0.0);
        formats = dash_formats(&manifest, &root.url, &headers_for, played);
    } else if hls::is_master(&root.text) {
        let master = hls::master(&root.text, &root.url)?;
        duration = playlists[1..].iter().filter_map(|p| hls::read(&p.text, &p.url, &[]).ok()).map(|p| p.duration()).fold(0.0, f64::max);
        // A quality's sound is apart when the track its group starts with (the default one,
        // else the first) has a playlist of its own; otherwise that track is in the quality.
        let apart = |group: &Option<String>| {
            let mut tracks = master.audio.iter().filter(|a| Some(&a.group) == group.as_ref());
            let first = tracks.clone().find(|a| a.default).or_else(|| tracks.next());
            first.is_some_and(|a| a.url.is_some())
        };
        // The qualities with the most bits name the better group of tracks; in a group the
        // first track is the one players take.
        let best_bits = |track: &hls::AudioTrack| users(&master, track).filter_map(|v| v.bandwidth).max();
        let mut order: Vec<usize> = (0..master.audio.len()).collect();
        order.sort_by_key(|&i| (best_bits(&master.audio[i]), std::cmp::Reverse(i)));
        for (i, track) in master.audio.iter().enumerate() {
            let Some(url) = &track.url else { continue };
            formats.push(json!({
                "format_id": format!("a{i}"),
                "ext": "mp4",
                "protocol": "m3u8_native",
                "url": url.as_str(),
                "http_headers": headers_for(url),
                "vcodec": "none",
                "acodec": users(&master, track).find_map(|v| v.codecs.iter().find(|c| is_audio(c))),
                "format_note": track.name,
                "language_preference": i32::from(track.default),
                "source_preference": order.iter().position(|&o| o == i),
            }));
        }
        for (i, v) in master.variants.iter().enumerate() {
            let picture = v.codecs.iter().find(|c| is_video(c));
            let sound = v.codecs.iter().find(|c| is_audio(c));
            // Codecs that are all sound, and no picture size: an audio-only quality.
            let sound_alone = picture.is_none() && sound.is_some() && v.resolution.is_none();
            formats.push(json!({
                "format_id": format!("v{i}"),
                "ext": "mp4",
                "protocol": "m3u8_native",
                "url": v.url.as_str(),
                "http_headers": headers_for(&v.url),
                "tbr": v.bandwidth.map(|b| b as f64 / 1000.0),
                "width": v.resolution.map(|r| r.0),
                "height": v.resolution.map(|r| r.1),
                "fps": v.frame_rate,
                "vcodec": if sound_alone { json!("none") } else { json!(picture) },
                "acodec": if apart(&v.audio) { json!("none") } else { json!(sound) },
                "dynamic_range": match v.video_range.as_deref() {
                    Some("PQ") => Some("HDR10"),
                    Some("HLG") => Some("HLG"),
                    _ => None,
                },
                "picture": !sound_alone,
            }));
        }
    } else {
        let pieces = hls::read(&root.text, &root.url, &[])?;
        duration = pieces.duration();
        formats.push(json!({
            "format_id": "hls",
            "ext": "mp4",
            "protocol": "m3u8_native",
            "url": root.url,
            "http_headers": Value::Object(headers.iter().map(|(name, value)| (name.clone(), json!(value))).collect::<Map<_, _>>()),
            "filesize": pieces.exact_size(),
            "picture": true,
        }));
    }
    let title = Url::parse(&root.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.path_segments()?.next_back().map(|name| name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_owned()))
        .filter(|stem| !stem.is_empty());
    Ok(serde_json::to_vec(&json!({
        "title": title,
        "duration": Some(duration).filter(|d| *d > 0.0),
        "formats": formats,
    }))?)
}

/// A manifest's streams as yt-dlp lists them: each with its pieces, or as one file. The
/// pieces are asked for as the manifest at `root` was (cookies stay with its host).
fn dash_formats(manifest: &dash::Manifest, root: &str, headers_for: &dyn Fn(&Url) -> Value, played: &HashSet<String>) -> Vec<Value> {
    let audio = || manifest.streams.iter().filter(|s| s.kind == dash::Kind::Audio);
    let sound_apart = audio().next().is_some();
    let count = manifest.streams.len();
    // The language to pair a picture with: the one the tab loaded, else the one the
    // manifest marks as its main one.
    let loaded = |s: &dash::Stream| s.file.iter().chain(s.pieces.iter().map(|p| &p.url)).any(|u| played.contains(&same(u.as_str())));
    let languages = |of: &dyn Fn(&dash::Stream) -> bool| -> HashSet<&str> { audio().filter(|s| of(s)).filter_map(|s| s.language.as_deref()).collect() };
    let mut wanted = if played.is_empty() { HashSet::new() } else { languages(&loaded) };
    if wanted.is_empty() {
        wanted = languages(&|s| s.role.as_deref() == Some("main"));
    }
    let mut formats = Vec::new();
    for (i, s) in manifest.streams.iter().enumerate() {
        let codecs: Vec<&str> = s.codecs.as_deref().map(|c| c.split(',').map(str::trim).collect()).unwrap_or_default();
        let picture = codecs.iter().find(|c| is_video(c));
        let sound = codecs.iter().find(|c| is_audio(c));
        let video = s.kind == dash::Kind::Video;
        let ext = match (s.mime.ends_with("/webm"), video) {
            (true, _) => "webm",
            (false, true) => "mp4",
            (false, false) => "m4a",
        };
        let mut format = json!({
            "format_id": format!("d{i}"),
            "ext": ext,
            "tbr": s.bandwidth.map(|b| b as f64 / 1000.0),
            "width": s.resolution.map(|r| r.0),
            "height": s.resolution.map(|r| r.1),
            "fps": s.frame_rate,
            "vcodec": if video { json!(picture) } else { json!("none") },
            // A picture carries its sound when its codecs name one; when they do not and
            // the manifest has audio streams, the sound is one of those.
            "acodec": if video && sound.is_none() && sound_apart { json!("none") } else { json!(sound) },
            "format_note": s.language,
            // First a track in the wanted language, then the set marked as the main one,
            // last what is marked as something aside (a dub, a commentary); of equals the
            // set named first, as players start with it. In a set the bit rate decides.
            "language_preference": match (s.role.as_deref(), s.language.as_deref()) {
                (Some(role), _) if role != "main" => -1,
                (_, Some(language)) if wanted.contains(language) => 2,
                (Some(_), _) => 1,
                (None, _) => 0,
            } * 1_000_000 - s.set.min(999_999) as i32,
            "source_preference": count - i,
            "picture": video,
        });
        let delivery = match &s.file {
            Some(file) => json!({ "protocol": "https", "url": file.as_str(), "http_headers": headers_for(file) }),
            None => {
                let pieces: Vec<Value> = s
                    .pieces
                    .iter()
                    .map(|p| {
                        json!({
                            "url": p.url.as_str(),
                            "byte_range": p.range.map(|(start, length)| json!({ "start": start, "end": start + length })),
                            "duration": Some(p.duration).filter(|d| *d > 0.0),
                        })
                    })
                    .collect();
                let headers = Url::parse(root).map_or(Value::Null, |u| headers_for(&u));
                json!({ "protocol": "http_dash_segments", "url": root, "http_headers": headers, "fragments": pieces })
            }
        };
        if let (Some(format), Value::Object(delivery)) = (format.as_object_mut(), delivery) {
            format.extend(delivery);
        }
        formats.push(format);
    }
    formats
}

/// The qualities whose sound `track`'s group carries.
fn users<'a>(master: &'a hls::Master, track: &'a hls::AudioTrack) -> impl Iterator<Item = &'a hls::Variant> {
    master.variants.iter().filter(move |v| v.audio.as_deref() == Some(track.group.as_str()))
}

fn is_video(codec: &str) -> bool {
    matches!(codec.split('.').next().unwrap_or(codec), "avc1" | "avc3" | "hvc1" | "hev1" | "dvh1" | "dvhe" | "av01" | "vp09" | "vp9" | "vp8")
}

fn is_audio(codec: &str) -> bool {
    matches!(codec.split('.').next().unwrap_or(codec), "mp4a" | "ac-3" | "ec-3" | "opus" | "Opus" | "flac" | "fLaC" | "alac" | "mp3" | "vorbis")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::Delivery;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"low\",NAME=\"English\",DEFAULT=YES,URI=\"a-low.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"high\",NAME=\"English\",DEFAULT=YES,URI=\"https://audio.example/a-high.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=900000,AVERAGE-BANDWIDTH=800000,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=640x360,FRAME-RATE=30,AUDIO=\"low\"\n\
        v360.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=6000000,CODECS=\"hvc1.2.4.L123.B0,ec-3\",RESOLUTION=1920x1080,FRAME-RATE=59.94,VIDEO-RANGE=PQ,AUDIO=\"high\"\n\
        v1080.m3u8?token=t\n";
    const QUALITY: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\ns0.ts\n#EXTINF:5.5,\ns1.ts\n#EXT-X-ENDLIST\n";

    fn caught(url: &str, kind: CaughtKind, text: Option<&str>) -> Caught {
        let headers = vec![("Referer".to_owned(), "https://site.example/".to_owned()), ("Cookie".to_owned(), "a=b".to_owned())];
        Caught { url: url.into(), kind, size: None, headers, text: text.map(str::to_owned), earlier: false }
    }

    fn lines(info: &VideoInfo) -> Vec<(&str, &str, &str, Option<u32>)> {
        info.choices.iter().map(|c| (c.format.as_str(), c.label.as_str(), c.codecs.as_str(), c.resolution)).collect()
    }

    #[test]
    fn a_master_is_listed_from_its_text_with_its_parts_folded_in() {
        let tab = [
            caught("https://cdn.example/v/v360.m3u8", CaughtKind::Hls, Some(QUALITY)),
            caught("https://cdn.example/v/master.m3u8#t=3", CaughtKind::Hls, Some(MASTER)),
            caught("https://cdn.example/v/s1.ts", CaughtKind::File, None),
            caught("https://cdn.example/v/other.mp4", CaughtKind::File, None),
            caught("https://cdn.example/v/master.m3u8?asked=again", CaughtKind::Hls, Some(MASTER)),
            caught("https://cdn.example/next-video/master.m3u8", CaughtKind::Hls, Some(MASTER)),
        ];
        let found = sources(&tab);
        // Not offered: the quality's playlist, its piece, and the master seen a second time.
        // The same text in another folder names other addresses: another video.
        assert_eq!(found.iter().map(|s| s.index).collect::<Vec<_>>(), [1, 3, 5]);
        let next = resolve(&saved(&tab, 5), &[], "v0").unwrap();
        assert_eq!(next.streams[0].url, "https://cdn.example/next-video/v360.m3u8");
        let info = found[0].info.as_ref().unwrap();
        assert_eq!(
            lines(info),
            [
                ("v1+a1", "1920x1080 60fps HDR", "H.265 + E-AC-3", Some(1080)),
                ("v0+a1", "640x360 30fps", "H.264 + E-AC-3", Some(360)),
                ("a1", "Audio (English)", "E-AC-3", None),
            ],
            "the track of the quality with the most bits is the one paired"
        );
        // The length comes from the quality the page played; 800 kbit/s over 15.5 s.
        assert_eq!((info.title.as_str(), info.duration), ("master", Some(15.5)));
        assert!(found[1].info.is_none() && found[1].kind == CaughtKind::File);

        let kept = saved(&tab, 1);
        assert_eq!(kept.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(), ["https://cdn.example/v/master.m3u8#t=3", "https://cdn.example/v/v360.m3u8"]);
        assert_eq!(text_of(&kept, "https://cdn.example/v/v360.m3u8"), Some(QUALITY));
        assert_eq!(text_of(&kept, "https://cdn.example/v/v1080.m3u8?token=t"), None);
        assert!(saved(&tab, 3).is_empty() && saved(&tab, 9).is_empty());

        let r = resolve(&kept, &tab[1].headers, "v0+a1").unwrap();
        assert_eq!(r.duration, Some(15.5));
        let streams: Vec<(&str, &str, &Delivery)> = r.streams.iter().map(|s| (s.format_id.as_str(), s.url.as_str(), &s.delivery)).collect();
        assert_eq!(
            streams,
            [("v0", "https://cdn.example/v/v360.m3u8", &Delivery::Hls), ("a1", "https://audio.example/a-high.m3u8", &Delivery::Hls)]
        );
        assert_eq!(r.streams[0].size_hint, Some((800.0 * 125.0 * 15.5) as u64));
        // The cookie goes to the playlist's own host only, as in the browser.
        let names = |i: usize| r.streams[i].headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>();
        assert_eq!((names(0), names(1)), (vec!["Cookie", "Referer"], vec!["Referer"]));
        assert!(resolve(&kept, &[], "v7").unwrap_err().to_string().contains("no quality v7"));
    }

    #[test]
    fn a_media_playlist_alone_is_one_line() {
        let ranges = "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"main.mp4\",BYTERANGE=\"700@0\"\n\
            #EXTINF:6,\n#EXT-X-BYTERANGE:1000@700\nmain.mp4\n#EXTINF:6,\n#EXT-X-BYTERANGE:300\nmain.mp4\n";
        let tab = [
            caught("https://cdn.example/x/main.mp4", CaughtKind::File, None),
            caught("https://cdn.example/x/a1b2", CaughtKind::Hls, Some(ranges)),
        ];
        let found = sources(&tab);
        assert_eq!(found.len(), 1, "the file is the playlist's own pieces");
        let info = found[0].info.as_ref().unwrap();
        assert_eq!(lines(info), [("hls", "Video", "", Some(0))]);
        let line = &info.choices[0];
        assert_eq!((line.size, line.approx, line.stream.as_deref(), info.duration), (Some(2000), false, Some("HLS"), Some(12.0)));
        let kept = saved(&tab, 1);
        let r = resolve(&kept, &tab[1].headers, "hls").unwrap();
        assert_eq!((r.streams[0].url.as_str(), r.streams[0].headers.len()), ("https://cdn.example/x/a1b2", 2));
        assert_eq!(text_of(&kept, &r.streams[0].url), Some(ranges));
    }

    /// Two picture sizes in numbered pieces, and the sound as one file on another host.
    const MANIFEST: &str = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT10S"><Period>
  <AdaptationSet mimeType="video/mp4" frameRate="25">
    <SegmentTemplate timescale="1" duration="4" media="$RepresentationID$/$Number$.m4s" initialization="$RepresentationID$/init.mp4"/>
    <Representation id="low" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e"/>
    <Representation id="high" bandwidth="2400000" width="1280" height="720" codecs="avc1.64001f"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" lang="en" codecs="mp4a.40.2">
    <Representation id="en" bandwidth="128000"><BaseURL>https://audio.example/en.m4a</BaseURL><SegmentBase indexRange="0-99"/></Representation>
  </AdaptationSet>
</Period></MPD>"#;

    #[test]
    fn a_dash_manifest_is_listed_from_its_text_with_its_pieces_folded_in() {
        let tab = [
            caught("https://cdn.example/v/low/2.m4s", CaughtKind::File, None),
            caught("https://audio.example/en.m4a", CaughtKind::File, None),
            caught("https://cdn.example/v/all.mpd?once=1", CaughtKind::Dash, Some(MANIFEST)),
            caught("https://cdn.example/v/other.mp4", CaughtKind::File, None),
            caught("https://cdn.example/v/all.mpd?once=2", CaughtKind::Dash, Some(MANIFEST)),
        ];
        let found = sources(&tab);
        // Not offered: a piece, the sound's file, and the manifest asked for a second time.
        assert_eq!(found.iter().map(|s| (s.index, s.kind)).collect::<Vec<_>>(), [(2, CaughtKind::Dash), (3, CaughtKind::File)]);
        let info = found[0].info.as_ref().unwrap();
        assert_eq!(
            lines(info),
            [("d1+d2", "1280x720 25fps", "H.264 + AAC", Some(720)), ("d0+d2", "640x360 25fps", "H.264 + AAC", Some(360)), ("d2", "Audio 128 kbps", "AAC", None)]
        );
        assert_eq!((info.title.as_str(), info.duration), ("all", Some(10.0)));
        // Bit rate x length: (2400 + 128) kbit/s for 10 s.
        assert_eq!((info.choices[0].size, info.choices[0].approx, info.choices[0].stream.as_deref()), (Some(3_160_000), true, Some("DASH")));

        let kept = saved(&tab, 2);
        assert_eq!(kept.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(), ["https://cdn.example/v/all.mpd?once=1"]);
        let r = resolve(&kept, &tab[2].headers, "d0+d2").unwrap();
        let [picture, sound] = r.streams.as_slice() else { panic!("{:?}", r.streams) };
        // The stream is asked for as the manifest was, cookie and all (which piece's host
        // gets the cookie is the engine's rule).
        assert_eq!((picture.url.as_str(), picture.ext.as_str(), picture.headers.len()), ("https://cdn.example/v/all.mpd?once=1", "mp4", 2));
        let Delivery::Dash(pieces) = &picture.delivery else { panic!("{:?}", picture.delivery) };
        assert_eq!(
            pieces.iter().map(|(url, range)| (url.as_str(), *range)).collect::<Vec<_>>(),
            [
                ("https://cdn.example/v/low/init.mp4", None),
                ("https://cdn.example/v/low/1.m4s", None),
                ("https://cdn.example/v/low/2.m4s", None),
                ("https://cdn.example/v/low/3.m4s", None),
            ]
        );
        assert_eq!(picture.size_hint, Some(1_000_000));
        // One file on another host: a plain download, without the cookie.
        assert_eq!((sound.url.as_str(), sound.ext.as_str(), &sound.delivery), ("https://audio.example/en.m4a", "m4a", &Delivery::File));
        assert_eq!(sound.headers, [("Referer".to_owned(), "https://site.example/".to_owned())]);
        assert!(resolve(&kept, &[], "v0").is_err());
    }

    #[test]
    fn the_sound_a_dash_picture_is_paired_with() {
        let manifest = |first: &str, second: &str| {
            format!(
                r#"<MPD mediaPresentationDuration="PT10S"><Period>
                <AdaptationSet mimeType="video/mp4" codecs="avc1.64001f"><Representation id="v" width="640" height="360"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet>
                <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2" lang="en">{first}
                  <Representation id="en-low" bandwidth="64000"><BaseURL>en64.m4a</BaseURL></Representation>
                  <Representation id="en" bandwidth="128000"><BaseURL>en128.m4a</BaseURL></Representation></AdaptationSet>
                <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2" lang="de">{second}
                  <Representation id="de" bandwidth="192000"><BaseURL>de192.m4a</BaseURL></Representation></AdaptationSet>
                </Period></MPD>"#
            )
        };
        let listed = |first: &str, second: &str, also: &str| {
            let tab = [caught("https://cdn.example/all.mpd", CaughtKind::Dash, Some(&manifest(first, second))), caught(also, CaughtKind::File, None)];
            let info = sources(&tab)[0].info.clone().unwrap();
            (info.choices[0].format.clone(), info.choices[1].label.clone())
        };
        let paired = |first: &str, second: &str| listed(first, second, "https://cdn.example/v.mp4");
        let (main, aside) = (r#"<Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>"#, r#"<Role schemeIdUri="urn:mpeg:dash:role:2011" value="commentary"/>"#);
        // The first set's best track, though the second set has more bits.
        assert_eq!(paired("", ""), ("d0+d2".to_owned(), "Audio 128 kbps".to_owned()));
        assert_eq!(paired(main, main), ("d0+d2".to_owned(), "Audio 128 kbps".to_owned()));
        assert_eq!(paired("", aside), ("d0+d2".to_owned(), "Audio 128 kbps".to_owned()));
        // A set marked as the main one comes before the first.
        assert_eq!(paired("", main), ("d0+d3".to_owned(), "Audio 192 kbps".to_owned()));
        assert_eq!(paired(aside, ""), ("d0+d3".to_owned(), "Audio 192 kbps".to_owned()));
        // The language the tab loaded comes before all of that; its best track is taken,
        // not the one that was loaded.
        assert_eq!(listed("", "", "https://cdn.example/de192.m4a"), ("d0+d3".to_owned(), "Audio 192 kbps".to_owned()));
        assert_eq!(listed("", main, "https://cdn.example/en64.m4a"), ("d0+d2".to_owned(), "Audio 128 kbps".to_owned()));
    }

    #[test]
    fn dash_pieces_with_byte_ranges_and_a_manifest_the_page_built() {
        let built = "blob:https://site.example/7-8-9";
        let text = r#"<MPD mediaPresentationDuration="PT8S"><Period><AdaptationSet mimeType="video/webm" codecs="vp09.00.30.08, opus">
            <Representation id="v" bandwidth="1000000" width="1920" height="1080"><BaseURL>https://cdn.example/one.webm</BaseURL>
            <SegmentList duration="4"><Initialization range="0-99"/><SegmentURL mediaRange="100-599"/><SegmentURL mediaRange="600-999"/></SegmentList>
            </Representation></AdaptationSet></Period></MPD>"#;
        let tab = [caught("https://cdn.example/one.webm", CaughtKind::File, None), caught(built, CaughtKind::Dash, Some(text))];
        let found = sources(&tab);
        assert_eq!(found.iter().map(|s| s.index).collect::<Vec<_>>(), [1]);
        // The picture's own codecs name its sound: one stream, no track to add.
        assert_eq!(lines(found[0].info.as_ref().unwrap()), [("d0", "1920x1080", "VP9 + Opus", Some(1080))]);
        let r = resolve(&saved(&tab, 1), &tab[1].headers, "d0").unwrap();
        assert_eq!((r.streams[0].url.as_str(), r.streams[0].ext.as_str()), (built, "webm"));
        let Delivery::Dash(pieces) = &r.streams[0].delivery else { panic!("{:?}", r.streams[0].delivery) };
        assert_eq!(pieces.iter().map(|(_, range)| *range).collect::<Vec<_>>(), [Some((0, 100)), Some((100, 500)), Some((600, 400))]);
        // DRM is refused, not listed.
        let drm = text.replace("<Representation", r#"<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><Representation"#);
        let tab = [caught("https://cdn.example/drm.mpd", CaughtKind::Dash, Some(&drm))];
        assert!(sources(&tab)[0].info.is_none() && saved(&tab, 0).is_empty());
    }

    #[test]
    fn what_cannot_be_listed_from_its_text_goes_by_its_address() {
        let live = "#EXTM3U\n#EXTINF:4,\ns9.ts\n";
        let built = "blob:https://site.example/1-2-3";
        let tab = [
            caught("https://cdn.example/live.m3u8", CaughtKind::Hls, Some(live)),
            caught("https://cdn.example/plain.m3u8", CaughtKind::Hls, None),
            caught("https://cdn.example/manifest.mpd", CaughtKind::Dash, Some(r#"<MPD type="dynamic"><Period/></MPD>"#)),
            caught(built, CaughtKind::Hls, Some("#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/s0.ts\n#EXT-X-ENDLIST\n")),
            caught("blob:https://site.example/4-5-6", CaughtKind::Hls, Some("#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/live9.ts\n")),
            caught("https://cdn.example/answers-to-post-only", CaughtKind::Hls, Some("#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/s0.ts\n#EXT-X-ENDLIST\n")),
            caught("https://cdn.example/live-copy", CaughtKind::Hls, Some("#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/live9.ts\n")),
        ];
        let found = sources(&tab);
        // The live playlist the page built can be neither listed nor asked for: left out,
        // and it does not hide its copy that has an address. The built one that can be
        // listed stands for its copy read from the network: one video.
        assert_eq!(
            found.iter().map(|s| (s.index, s.info.is_some())).collect::<Vec<_>>(),
            [(0, false), (1, false), (2, false), (3, true), (6, false)]
        );
        assert!(saved(&tab, 0).is_empty() && saved(&tab, 1).is_empty() && saved(&tab, 2).is_empty());
        // One the page built: the title is no address, the piece is.
        let info = found[3].info.as_ref().unwrap();
        assert_eq!((info.title.as_str(), info.duration), ("video", Some(4.0)));
        let r = resolve(&saved(&tab, 3), &[], "hls").unwrap();
        assert_eq!(r.streams[0].url, built);
        assert_eq!(engine::hls::read(text_of(&saved(&tab, 3), built).unwrap(), built, &[]).unwrap().pieces[0].url.as_str(), "https://cdn.example/s0.ts");
    }

    /// A quality's sound is in it or apart, by the track its group starts with; among
    /// tracks that are apart, the first is the one paired.
    #[test]
    fn the_track_players_start_with_decides_the_pairing() {
        let inside = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"English\",DEFAULT=YES\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Deutsch\",URI=\"de.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"a\"\nv.m3u8\n";
        let found = sources(&[caught("https://cdn.example/m.m3u8", CaughtKind::Hls, Some(inside))]);
        // English is in the quality itself: taking the German track instead would lose it.
        assert_eq!(
            lines(found[0].info.as_ref().unwrap()),
            [("v0", "1280x720", "H.264 + AAC", Some(720)), ("a1", "Audio (Deutsch)", "AAC", None)]
        );
        let no_default = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Original\",URI=\"o.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"Audio description\",URI=\"d.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO=\"a\"\nv.m3u8\n";
        let found = sources(&[caught("https://cdn.example/m.m3u8", CaughtKind::Hls, Some(no_default))]);
        assert_eq!(
            lines(found[0].info.as_ref().unwrap()),
            [("v0+a0", "1280x720", "", Some(720)), ("a0", "Audio (Original)", "", None)]
        );
    }

    #[test]
    fn qualities_without_a_picture_size_and_sound_alone() {
        let old = "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1280000\nmid.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\naudio.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2560000,CODECS=\"avc1.4d401f\"\nhigh.m3u8\n";
        let tab = [caught("https://cdn.example/old.m3u8", CaughtKind::Hls, Some(old))];
        let found = sources(&tab);
        let info = found[0].info.as_ref().unwrap();
        // A picture codec alone says nothing about sound: the quality is offered as it is.
        assert_eq!(
            lines(info),
            [("v2", "2560 kbps", "H.264", Some(0)), ("v0", "1280 kbps", "", Some(0)), ("v1", "Audio 64 kbps", "AAC", None)]
        );
        assert_eq!(info.duration, None);
    }
}
