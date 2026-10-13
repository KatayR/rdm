//! DASH manifests (MPD): the streams one offers and the pieces each is made of.
//!
//! Read from the text the browser's page fetched, so the manifest's address is not asked
//! again (some answer once, or only to the page). A manifest of a finished video with one
//! period is read: pieces named by a template (numbered, or timed along a timeline) or by
//! a list, or one file per stream. Live manifests, several periods and DRM are refused.

use std::cell::Cell;

use reqwest::Url;
use roxmltree::{Document, Node};

use crate::{Error, Result};

/// Lengths and piece lengths are divided as written: this much of a piece over a whole
/// number of pieces is rounding, not a piece more.
const PIECE_SLACK: f64 = 1e-6;
/// More pieces than the streams of any real video have together; a manifest that asks for
/// more is not believed.
const MAX_PIECES: u64 = 200_000;
/// Manifests nest about six elements deep. The XML reader calls itself for every level, so
/// text nested much deeper (a page can send any) is refused before it is read.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Video,
    Audio,
}

/// One piece of a stream: a whole file, or a byte range of one.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub url: Url,
    /// `(start, length)`.
    pub range: Option<(u64, u64)>,
    /// Seconds of media; 0 for an initialisation section.
    pub duration: f64,
}

/// One quality or audio track (a `Representation`).
#[derive(Debug, Clone)]
pub struct Stream {
    pub kind: Kind,
    /// `video/mp4`, `audio/webm`.
    pub mime: String,
    /// As the manifest writes them: `avc1.64001f`, or `avc1.64001f,mp4a.40.2` for a
    /// picture with its sound.
    pub codecs: Option<String>,
    /// Bits per second.
    pub bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    pub language: Option<String>,
    /// What its set is for, when the manifest says: `main`, `dub`, `commentary`.
    pub role: Option<String>,
    /// The place of its set in the manifest; players start with the first one.
    pub set: usize,
    /// The stream as one file, when the manifest names no pieces.
    pub file: Option<Url>,
    /// The initialisation section, then the pieces in playing order. Empty with `file`.
    pub pieces: Vec<Piece>,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    /// Seconds.
    pub duration: Option<f64>,
    pub streams: Vec<Stream>,
}

/// True for text that begins as a DASH manifest.
pub fn is_manifest(text: &str) -> bool {
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    // An XML declaration and comments may come first.
    while let Some(end) = [("<?", "?>"), ("<!--", "-->")].iter().find(|(start, _)| rest.starts_with(start)).map(|(_, end)| end) {
        let Some((_, after)) = rest.split_once(end) else { return false };
        rest = after.trim_start();
    }
    rest.strip_prefix("<MPD").is_some_and(|r| r.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/'))
}

/// The streams of the manifest `text`, which was read from `url`.
pub fn read(text: &str, url: &str) -> Result<Manifest> {
    let base = address(url)?;
    let doc = parse(text)?;
    let mpd = doc.root_element();
    if mpd.attribute("type") == Some("dynamic") {
        return Err(Error::Live);
    }
    if mpd.descendants().any(|n| n.is_element() && n.tag_name().name() == "ContentProtection") {
        return Err(Error::Protected("DASH content protection".into()));
    }
    let periods: Vec<Node> = children(mpd, "Period").collect();
    let [period] = periods.as_slice() else {
        return Err(Error::Playlist(format!("a DASH manifest with {} periods", periods.len())));
    };
    let whole = mpd.attribute("mediaPresentationDuration").and_then(seconds);
    let duration = period.attribute("duration").and_then(seconds).or(whole);
    let left = Cell::new(MAX_PIECES);
    let mut streams = Vec::new();
    for (place, set, rep) in representations(*period) {
        // Pictures for fast forward are no quality to offer.
        let trick = children(set, "EssentialProperty").any(|p| p.attribute("schemeIdUri").is_some_and(|s| s.contains("trickmode")));
        let Some(kind) = kind_of(set, rep).filter(|_| !trick) else { continue };
        let (file, pieces) = delivery([mpd, *period, set, rep], &base, duration, &left)?;
        let at = |name: &str| rep.attribute(name).or_else(|| set.attribute(name));
        streams.push(Stream {
            kind,
            mime: at("mimeType").unwrap_or_default().to_owned(),
            codecs: at("codecs").map(str::to_owned),
            bandwidth: rep.attribute("bandwidth").and_then(|b| b.parse().ok()),
            resolution: at("width").and_then(|w| w.parse().ok()).zip(at("height").and_then(|h| h.parse().ok())),
            frame_rate: at("frameRate").and_then(frame_rate),
            language: at("lang").map(str::to_owned),
            role: child(set, "Role").and_then(|r| r.attribute("value")).map(str::to_owned),
            set: place,
            file,
            pieces,
        });
    }
    if streams.is_empty() {
        return Err(Error::Playlist("the DASH manifest lists no video or audio".into()));
    }
    Ok(Manifest { duration, streams })
}

/// Every address the manifest `text` (from `url`) names: the files and pieces of everything
/// in it (subtitles and the like too), in all periods, also of a manifest `read` refuses.
/// What cannot be read is left out.
pub fn named(text: &str, url: &str) -> Vec<Url> {
    let (Ok(base), Ok(doc)) = (Url::parse(url), parse(text)) else { return Vec::new() };
    let mpd = doc.root_element();
    let whole = mpd.attribute("mediaPresentationDuration").and_then(seconds);
    let left = Cell::new(MAX_PIECES);
    let mut out = Vec::new();
    for period in children(mpd, "Period") {
        let duration = period.attribute("duration").and_then(seconds).or(whole);
        for (_, set, rep) in representations(period) {
            if let Ok((file, pieces)) = delivery([mpd, period, set, rep], &base, duration, &left) {
                out.extend(file);
                out.extend(pieces.into_iter().map(|p| p.url));
            }
        }
    }
    out
}

fn address(url: &str) -> Result<Url> {
    Url::parse(url).map_err(|_| Error::Playlist(format!("bad manifest address {url}")))
}

fn parse(text: &str) -> Result<Document<'_>> {
    if depth(text) > MAX_DEPTH {
        return Err(Error::Playlist("not a DASH manifest: its elements nest too deep".into()));
    }
    let doc = Document::parse(text.trim_start_matches('\u{feff}')).map_err(|e| Error::Playlist(format!("not a DASH manifest: {e}")))?;
    match doc.root_element().tag_name().name() {
        "MPD" => Ok(doc),
        other => Err(Error::Playlist(format!("not a DASH manifest: it starts with <{other}>"))),
    }
}

/// How deep the elements of `text` nest at most.
fn depth(text: &str) -> usize {
    let (mut depth, mut deepest) = (0usize, 0);
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        rest = &rest[at..];
        let skipped = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>")].iter().find(|(start, _)| rest.starts_with(start));
        if let Some((_, end)) = skipped {
            rest = rest.find(end).map_or("", |i| &rest[i + end.len()..]);
            continue;
        }
        // To the tag's end; a `>` inside a quoted value does not end it.
        let mut quote = None;
        let mut end = rest.len();
        for (i, c) in rest.char_indices().skip(1) {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"' | '\'') => quote = Some(c),
                (None, '>') => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let tag = &rest[..end];
        if tag.starts_with("</") {
            depth = depth.saturating_sub(1);
        } else if !tag.starts_with("<!") && !tag.ends_with('/') {
            depth += 1;
            deepest = deepest.max(depth);
        }
        rest = rest.get(end + 1..).unwrap_or("");
    }
    deepest
}

/// Every representation of a period, with its set and the set's place in the period.
fn representations<'a>(period: Node<'a, 'a>) -> impl Iterator<Item = (usize, Node<'a, 'a>, Node<'a, 'a>)> {
    children(period, "AdaptationSet").enumerate().flat_map(|(place, set)| children(set, "Representation").map(move |rep| (place, set, rep)))
}

/// Video or audio; None for anything else (subtitles, thumbnails).
fn kind_of(set: Node, rep: Node) -> Option<Kind> {
    let at = |name: &str| rep.attribute(name).or_else(|| set.attribute(name));
    let component = || child(set, "ContentComponent").and_then(|c| c.attribute("contentType"));
    let mime = at("mimeType").unwrap_or_default();
    match at("contentType").or_else(component).unwrap_or_else(|| mime.split('/').next().unwrap_or_default()) {
        "video" => Some(Kind::Video),
        "audio" => Some(Kind::Audio),
        _ => None,
    }
}

/// How one representation is delivered: as one file, or in pieces. `levels` are the
/// manifest, the period, the set and the representation itself; `length` is the period's,
/// in seconds; `left` is how many pieces the manifest may still name.
fn delivery(levels: [Node; 4], root: &Url, length: Option<f64>, left: &Cell<u64>) -> Result<(Option<Url>, Vec<Piece>)> {
    let [.., rep] = levels;
    let mut base = root.clone();
    for level in levels {
        if let Some(own) = base_of(level) {
            base = base.join(own).map_err(|_| Error::Playlist(format!("bad DASH base address {own}")))?;
        }
    }
    let bandwidth = rep.attribute("bandwidth").and_then(|b| b.parse().ok());
    if let Some(pieces) = pieces(&levels[1..], &base, rep.attribute("id").unwrap_or_default(), bandwidth, length, left)? {
        return Ok((None, pieces));
    }
    // One file: some level has to name it, and a folder or the manifest itself is none.
    if base == *root || base.path().ends_with('/') {
        return Err(Error::Playlist("a DASH stream without pieces or a file".into()));
    }
    Ok((Some(base), Vec::new()))
}

/// The element's own `BaseURL`, when it has one that says something.
fn base_of<'a>(node: Node<'a, 'a>) -> Option<&'a str> {
    child(node, "BaseURL").and_then(|b| b.text()).map(str::trim).filter(|text| !text.is_empty())
}

/// The pieces a representation's template or list names; None when it has neither (the
/// stream is one file). `levels` are its period, set and the representation itself: each
/// may say part of it, the inner one counts.
fn pieces(levels: &[Node], base: &Url, id: &str, bandwidth: Option<u64>, length: Option<f64>, left: &Cell<u64>) -> Result<Option<Vec<Piece>>> {
    let templates: Vec<Node> = levels.iter().filter_map(|n| child(*n, "SegmentTemplate")).collect();
    if !templates.is_empty() {
        let attr = |name: &str| templates.iter().rev().find_map(|t| t.attribute(name));
        let timeline = templates.iter().rev().find_map(|t| child(*t, "SegmentTimeline"));
        return from_template(&attr, timeline, base, id, bandwidth, length, left).map(Some);
    }
    let lists: Vec<Node> = levels.iter().filter_map(|n| child(*n, "SegmentList")).collect();
    let Some(list) = lists.iter().rev().find(|l| child(**l, "SegmentURL").is_some()) else {
        return match lists.is_empty() {
            true => Ok(None),
            false => Err(Error::Playlist("a DASH piece list without pieces".into())),
        };
    };
    let attr = |name: &str| lists.iter().rev().find_map(|l| l.attribute(name));
    let timescale = number(attr("timescale"), 1.0)?.max(f64::MIN_POSITIVE);
    let each = number(attr("duration"), 0.0)? / timescale;
    let mut out = Vec::new();
    if let Some(init) = lists.iter().rev().find_map(|l| child(*l, "Initialization")) {
        out.push(Piece { url: join(base, init.attribute("sourceURL"))?, range: range(init.attribute("range"))?, duration: 0.0 });
    }
    take(left, children(*list, "SegmentURL").count() as u64)?;
    for piece in children(*list, "SegmentURL") {
        out.push(Piece { url: join(base, piece.attribute("media"))?, range: range(piece.attribute("mediaRange"))?, duration: each });
    }
    Ok(Some(out))
}

/// Counts `count` more pieces against what the manifest may name.
fn take(left: &Cell<u64>, count: u64) -> Result<()> {
    let rest = left.get().checked_sub(count).ok_or_else(|| Error::Playlist("a DASH manifest with too many pieces".into()))?;
    left.set(rest);
    Ok(())
}

fn from_template<'a>(
    attr: &dyn Fn(&str) -> Option<&'a str>,
    timeline: Option<Node>,
    base: &Url,
    id: &str,
    bandwidth: Option<u64>,
    length: Option<f64>,
    left: &Cell<u64>,
) -> Result<Vec<Piece>> {
    let media = attr("media").ok_or_else(|| Error::Playlist("a DASH template without media addresses".into()))?;
    let timescale = number(attr("timescale"), 1.0)?.max(f64::MIN_POSITIVE);
    let first = number(attr("startNumber"), 1.0)? as u64;
    let at = |template: &str, number: u64, time: u64| join(base, Some(&expand(template, id, bandwidth, number, time)?));
    let mut out = Vec::new();
    if let Some(init) = attr("initialization") {
        out.push(Piece { url: at(init, first, 0)?, range: None, duration: 0.0 });
    }
    match timeline {
        Some(timeline) => {
            let rows: Vec<Node> = children(timeline, "S").collect();
            // Times count from the template's offset, so the period ends that much later.
            let end = length.map(|l| (number(attr("presentationTimeOffset"), 0.0).unwrap_or(0.0) + l * timescale) as u64);
            let (mut n, mut time) = (first, 0u64);
            for (i, row) in rows.iter().enumerate() {
                let whole = |name: &str| row.attribute(name).map(|v| v.parse::<i64>().map_err(|_| Error::Playlist(format!("bad DASH timeline value {v}"))));
                time = whole("t").transpose()?.map_or(time, |t| t.max(0) as u64);
                let d = whole("d").transpose()?.filter(|d| *d > 0).ok_or_else(|| Error::Playlist("a DASH timeline row without a length".into()))? as u64;
                // A negative repeat count: until the next row starts, or the period ends.
                let count = match whole("r").transpose()?.unwrap_or(0) {
                    r if r >= 0 => r as u64 + 1,
                    _ => {
                        let next = rows.get(i + 1).and_then(|s| s.attribute("t")).and_then(|t| t.parse::<u64>().ok());
                        let until = next.or(end).ok_or_else(|| Error::Playlist("a DASH timeline without an end".into()))?;
                        until.saturating_sub(time).div_ceil(d)
                    }
                };
                take(left, count)?;
                for _ in 0..count {
                    out.push(Piece { url: at(media, n, time)?, range: None, duration: d as f64 / timescale });
                    n = n.saturating_add(1);
                    time = time.saturating_add(d);
                }
            }
        }
        None => {
            let d = number(attr("duration"), 0.0)?;
            if d <= 0.0 {
                return Err(Error::Playlist("a DASH template without piece lengths".into()));
            }
            let count = match attr("endNumber").and_then(|e| e.parse::<u64>().ok()) {
                Some(last) => last.saturating_add(1).saturating_sub(first),
                None => {
                    let length = length.ok_or_else(|| Error::Playlist("a DASH manifest without a length".into()))?;
                    (length * timescale / d - PIECE_SLACK).ceil().max(0.0) as u64
                }
            };
            take(left, count)?;
            let each = d / timescale;
            for i in 0..count {
                let rest = length.map_or(each, |l| (l - i as f64 * each).clamp(0.0, each));
                out.push(Piece { url: at(media, first.saturating_add(i), (i as f64 * d) as u64)?, range: None, duration: rest });
            }
        }
    }
    Ok(out)
}

/// A template with its `$...$` names filled in: `$RepresentationID$`, `$Bandwidth$`,
/// `$Number$`, `$Time$` (the last three also as `$Number%05d$`), and `$$` for a dollar.
fn expand(template: &str, id: &str, bandwidth: Option<u64>, number: u64, time: u64) -> Result<String> {
    let mut out = String::new();
    for (i, part) in template.split('$').enumerate() {
        if i % 2 == 0 {
            out.push_str(part);
            continue;
        }
        let (name, format) = part.split_once('%').map_or((part, None), |(n, f)| (n, Some(f)));
        let value = match name {
            "" if format.is_none() => {
                out.push('$');
                continue;
            }
            "RepresentationID" if format.is_none() => {
                out.push_str(id);
                continue;
            }
            "Number" => number,
            "Time" => time,
            "Bandwidth" => bandwidth.unwrap_or(0),
            _ => return Err(Error::Playlist(format!("DASH template name ${part}$ is not known"))),
        };
        // `%05d`, also `%d`; wider than any number is no real format.
        let width = match format.map(|f| (f, f.strip_suffix('d').map(|w| w.trim_start_matches('0')))) {
            None | Some((_, Some(""))) => 0,
            Some((f, digits)) => digits.and_then(|w| w.parse::<usize>().ok()).filter(|w| *w <= 20).ok_or_else(|| Error::Playlist(format!("bad DASH template format %{f}")))?,
        };
        out.push_str(&format!("{value:0width$}"));
    }
    Ok(out)
}

/// `address` seen from `base`; `base` itself when none is given.
fn join(base: &Url, address: Option<&str>) -> Result<Url> {
    match address {
        None => Ok(base.clone()),
        Some(a) => base.join(a).map_err(|_| Error::Playlist(format!("bad piece address {a}"))),
    }
}

/// `first-last` as `(start, length)`.
fn range(value: Option<&str>) -> Result<Option<(u64, u64)>> {
    let Some(value) = value else { return Ok(None) };
    let read = || {
        let (first, last) = value.split_once('-')?;
        let (first, last) = (first.trim().parse::<u64>().ok()?, last.trim().parse::<u64>().ok()?);
        Some((first, last.checked_add(1)?.checked_sub(first)?))
    };
    read().map(Some).ok_or_else(|| Error::Playlist(format!("bad byte range {value}")))
}

fn number(value: Option<&str>, default: f64) -> Result<f64> {
    match value {
        None => Ok(default),
        Some(v) => v.trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).ok_or_else(|| Error::Playlist(format!("bad DASH number {v}"))),
    }
}

/// `30`, or `30000/1001`.
fn frame_rate(value: &str) -> Option<f64> {
    let (frames, per) = value.split_once('/').unwrap_or((value, "1"));
    let rate = frames.trim().parse::<f64>().ok()? / per.trim().parse::<f64>().ok()?;
    rate.is_finite().then_some(rate)
}

/// An ISO 8601 length (`PT10M34.5S`, `P1DT2H`) in seconds.
fn seconds(iso: &str) -> Option<f64> {
    let rest = iso.trim().strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = 0.0;
    for (part, units) in [(date, [('Y', 31_536_000.0), ('M', 2_592_000.0), ('D', 86_400.0)]), (time, [('H', 3600.0), ('M', 60.0), ('S', 1.0)])] {
        let mut digits = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                digits.push(c);
            } else {
                let unit = units.iter().find(|(name, _)| *name == c)?.1;
                total += digits.parse::<f64>().ok()? * unit;
                digits.clear();
            }
        }
        if !digits.is_empty() {
            return None;
        }
    }
    Some(total)
}

fn child<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children().find(|n| n.is_element() && n.tag_name().name() == name)
}

fn children<'a>(node: Node<'a, 'a>, name: &'static str) -> impl Iterator<Item = Node<'a, 'a>> {
    node.children().filter(move |n| n.is_element() && n.tag_name().name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "https://cdn.example/v/manifest.mpd?token=1";

    fn urls(stream: &Stream) -> Vec<&str> {
        stream.pieces.iter().map(|p| p.url.as_str()).collect()
    }

    /// Numbered pieces of a fixed length, the template said once for the whole set.
    const NUMBERED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- made by a packager -->
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <BaseURL>media/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4" frameRate="30000/1001">
      <SegmentTemplate timescale="1000" duration="4000" startNumber="5" media="$RepresentationID$/seg-$Number%05d$.m4s?b=$Bandwidth$" initialization="$RepresentationID$/init.mp4"/>
      <Representation id="v1" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e"/>
      <Representation id="v2" bandwidth="2400000" width="1280" height="720" codecs="avc1.64001f">
        <SegmentTemplate media="hd/$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="tr" codecs="mp4a.40.2">
      <Role schemeIdUri="urn:mpeg:dash:role:2011" value="main"/>
      <SegmentTemplate timescale="1" duration="4" media="a/$Number$.m4s" initialization="a/init.mp4"/>
      <Representation id="a1" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet mimeType="text/vtt" lang="en"><Representation id="s"><BaseURL>subs.vtt</BaseURL></Representation></AdaptationSet>
    <AdaptationSet mimeType="video/mp4">
      <EssentialProperty schemeIdUri="http://dash-industry.org/2016:trickmode" value="1"/>
      <Representation id="trick" bandwidth="100"><BaseURL>trick.mp4</BaseURL></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn numbered_pieces_from_a_template() {
        assert!(is_manifest(NUMBERED) && is_manifest("\u{feff}  <MPD>") && !is_manifest("<MPDX>") && !is_manifest("#EXTM3U"));
        let m = read(NUMBERED, AT).unwrap();
        assert_eq!(m.duration, Some(10.0));
        // Subtitles and the fast-forward pictures are no streams.
        assert_eq!(m.streams.len(), 3);
        let v1 = &m.streams[0];
        assert_eq!((v1.kind, v1.mime.as_str(), v1.codecs.as_deref()), (Kind::Video, "video/mp4", Some("avc1.4d401e")));
        assert_eq!((v1.bandwidth, v1.resolution, v1.language.as_deref(), v1.role.as_deref(), v1.set), (Some(800_000), Some((640, 360)), None, None, 0));
        assert!((v1.frame_rate.unwrap() - 29.97).abs() < 0.001);
        // 10 s in pieces of 4 s: three, the last one 2 s long; numbers start at 5.
        assert_eq!(
            urls(v1),
            [
                "https://cdn.example/v/media/v1/init.mp4",
                "https://cdn.example/v/media/v1/seg-00005.m4s?b=800000",
                "https://cdn.example/v/media/v1/seg-00006.m4s?b=800000",
                "https://cdn.example/v/media/v1/seg-00007.m4s?b=800000",
            ]
        );
        assert_eq!(v1.pieces.iter().map(|p| p.duration).collect::<Vec<_>>(), [0.0, 4.0, 4.0, 2.0]);
        // The representation's own template changes the media address and keeps the rest.
        assert_eq!(urls(&m.streams[1])[..2], ["https://cdn.example/v/media/v2/init.mp4", "https://cdn.example/v/media/hd/5.m4s"]);
        let a = &m.streams[2];
        assert_eq!((a.kind, a.language.as_deref(), a.role.as_deref(), a.set, a.codecs.as_deref()), (Kind::Audio, Some("tr"), Some("main"), 1, Some("mp4a.40.2")));
        assert_eq!(urls(a), ["https://cdn.example/v/media/a/init.mp4", "https://cdn.example/v/media/a/1.m4s", "https://cdn.example/v/media/a/2.m4s", "https://cdn.example/v/media/a/3.m4s"]);
        // What is no stream to offer is still the manifest's: a tab that loaded it played no
        // file of its own.
        let all = named(NUMBERED, AT);
        assert_eq!(all.len(), 4 + 4 + 4 + 2);
        assert_eq!(all[12..].iter().map(Url::as_str).collect::<Vec<_>>(), ["https://cdn.example/v/media/subs.vtt", "https://cdn.example/v/media/trick.mp4"]);
    }

    #[test]
    fn a_whole_number_of_pieces_and_an_end_number() {
        let manifest = |extra: &str| {
            format!(
                r#"<MPD mediaPresentationDuration="PT0H0M12.000S"><Period><AdaptationSet mimeType="audio/webm">
                <SegmentTemplate duration="4" media="$Number$.webm" {extra}/><Representation id="a"/></AdaptationSet></Period></MPD>"#
            )
        };
        assert_eq!(read(&manifest(""), AT).unwrap().streams[0].pieces.len(), 3);
        assert_eq!(read(&manifest(r#"startNumber="0" endNumber="1""#), AT).unwrap().streams[0].pieces.len(), 2);
        // 16.1 s in pieces of 0.1 s: 16.1 * 1000 / 100 is a hair over 161 in floating point,
        // which is 161 pieces.
        let tenths = manifest(r#"timescale="1000""#).replace(r#"duration="4""#, r#"duration="100""#).replace("PT0H0M12.000S", "PT16.1S");
        assert_eq!(read(&tenths, AT).unwrap().streams[0].pieces.len(), 161);
        // A real rest is a piece: 12 s in pieces of 5 s are three.
        assert_eq!(read(&manifest("").replace(r#"duration="4""#, r#"duration="5""#), AT).unwrap().streams[0].pieces.len(), 3);
        // `%d` is a width like any other; the type is told by a component of the set.
        let typed = manifest("").replace(r#"mimeType="audio/webm""#, "").replace("<Representation", r#"<ContentComponent contentType="audio"/><Representation"#);
        let plain = typed.replace("$Number$", "$Number%d$-$Number%03d$");
        assert_eq!(read(&plain, AT).unwrap().streams[0].pieces[0].url.as_str(), "https://cdn.example/v/1-001.webm");
    }

    #[test]
    fn timed_pieces_along_a_timeline() {
        let text = r#"<MPD type="static" mediaPresentationDuration="PT20S"><Period duration="PT14S">
            <AdaptationSet mimeType="video/webm" codecs="vp09.00.30.08" width="1920" height="1080">
              <Representation id="1" bandwidth="1">
                <SegmentTemplate timescale="10" presentationTimeOffset="100" media="t$Time$-n$Number$-$$.webm" initialization="i-$RepresentationID$.webm">
                  <SegmentTimeline><S t="100" d="40" r="1"/><S d="20"/><S d="30" r="-1"/></SegmentTimeline>
                </SegmentTemplate>
              </Representation>
            </AdaptationSet></Period></MPD>"#;
        let m = read(text, AT).unwrap();
        // The period's own length counts, not the manifest's.
        assert_eq!(m.duration, Some(14.0));
        let s = &m.streams[0];
        // Two of 4 s from time 100, one of 2 s, then pieces of 3 s until the period ends
        // (14 s after time 100, so 240): at 200 and 230, the last one reaching past the end.
        assert_eq!(
            urls(s),
            [
                "https://cdn.example/v/i-1.webm",
                "https://cdn.example/v/t100-n1-$.webm",
                "https://cdn.example/v/t140-n2-$.webm",
                "https://cdn.example/v/t180-n3-$.webm",
                "https://cdn.example/v/t200-n4-$.webm",
                "https://cdn.example/v/t230-n5-$.webm",
            ]
        );
        assert_eq!(s.pieces.iter().map(|p| p.duration).collect::<Vec<_>>(), [0.0, 4.0, 4.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn a_timeline_row_repeated_until_the_next_one() {
        let text = r#"<MPD mediaPresentationDuration="PT9S"><Period><AdaptationSet mimeType="audio/mp4">
            <SegmentTemplate media="$Time$.m4s"><SegmentTimeline><S d="2" r="-1"/><S t="6" d="3"/></SegmentTimeline></SegmentTemplate>
            <Representation id="a"/></AdaptationSet></Period></MPD>"#;
        let m = read(text, AT).unwrap();
        assert_eq!(urls(&m.streams[0]), ["https://cdn.example/v/0.m4s", "https://cdn.example/v/2.m4s", "https://cdn.example/v/4.m4s", "https://cdn.example/v/6.m4s"]);
    }

    #[test]
    fn listed_pieces_and_single_files() {
        let text = r#"<MPD mediaPresentationDuration="PT8S"><Period>
            <AdaptationSet mimeType="video/mp4" codecs="avc1.64001f,mp4a.40.2">
              <Representation id="l" bandwidth="500000" width="640" height="360">
                <BaseURL>https://other.example/one.mp4</BaseURL>
                <SegmentList timescale="1000" duration="4000">
                  <Initialization range="0-862"/>
                  <SegmentURL mediaRange="863-7112"/><SegmentURL media="tail.m4s"/>
                </SegmentList>
              </Representation>
            </AdaptationSet>
            <AdaptationSet mimeType="audio/mp4">
              <Representation id="f" bandwidth="64000"><BaseURL>audio/track.m4a</BaseURL><SegmentBase indexRange="700-1000"/></Representation>
            </AdaptationSet></Period></MPD>"#;
        let m = read(text, AT).unwrap();
        let l = &m.streams[0];
        assert_eq!(l.file, None);
        assert_eq!(
            l.pieces,
            [
                Piece { url: Url::parse("https://other.example/one.mp4").unwrap(), range: Some((0, 863)), duration: 0.0 },
                Piece { url: Url::parse("https://other.example/one.mp4").unwrap(), range: Some((863, 6250)), duration: 4.0 },
                Piece { url: Url::parse("https://other.example/tail.m4s").unwrap(), range: None, duration: 4.0 },
            ]
        );
        let f = &m.streams[1];
        assert_eq!((f.file.as_ref().map(Url::as_str), f.pieces.len()), (Some("https://cdn.example/v/audio/track.m4a"), 0));
        // The set says how the file starts, the representation names its pieces.
        let split = text.replace(r#"<SegmentList timescale="1000" duration="4000">
                  <Initialization range="0-862"/>"#, r#"<SegmentList>"#).replace(r#"<Representation id="l""#, r#"<SegmentList timescale="1000" duration="4000"><Initialization range="0-862"/></SegmentList><Representation id="l""#);
        assert_ne!(split, text);
        assert_eq!(read(&split, AT).unwrap().streams[0].pieces, l.pieces);
        assert_eq!(
            named(text, AT).iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://other.example/one.mp4", "https://other.example/one.mp4", "https://other.example/tail.m4s", "https://cdn.example/v/audio/track.m4a"]
        );
    }

    #[test]
    fn a_manifest_the_page_built() {
        // Under a blob: address only whole addresses can be read.
        let text = |base: &str| format!(r#"<MPD mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="video/mp4">
            <Representation id="v">{base}<SegmentTemplate duration="4" media="p$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#);
        let blob = "blob:https://site.example/1-2-3";
        let m = read(&text("<BaseURL>https://cdn.example/x/</BaseURL>"), blob).unwrap();
        assert_eq!(urls(&m.streams[0]), ["https://cdn.example/x/p1.m4s"]);
        assert!(read(&text(""), blob).is_err());
        assert!(named(&text(""), blob).is_empty());
        // A base address that cannot be read from there is no file at the manifest's own
        // address, and an empty one or a folder names no file.
        let file = |base: &str| format!(r#"<MPD><Period><AdaptationSet mimeType="video/mp4"><Representation id="v">{base}</Representation></AdaptationSet></Period></MPD>"#);
        assert!(read(&file("<BaseURL>v.mp4</BaseURL>"), blob).is_err() && named(&file("<BaseURL>v.mp4</BaseURL>"), blob).is_empty());
        assert_eq!(read(&file("<BaseURL>v.mp4</BaseURL>"), AT).unwrap().streams[0].file.as_ref().unwrap().as_str(), "https://cdn.example/v/v.mp4");
        for none in ["<BaseURL> </BaseURL>", "<BaseURL>folder/</BaseURL>", ""] {
            assert!(read(&file(none), AT).is_err() && named(&file(none), AT).is_empty(), "{none}");
        }
    }

    #[test]
    fn what_is_refused() {
        let one = r#"<Period><AdaptationSet mimeType="video/mp4"><Representation id="v"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period>"#;
        assert!(read(&format!("<MPD>{one}</MPD>"), AT).is_ok());
        assert!(matches!(read(&format!(r#"<MPD type="dynamic">{one}</MPD>"#), AT), Err(Error::Live)));
        let two = read(&format!("<MPD>{one}{one}</MPD>"), AT).unwrap_err().to_string();
        assert!(two.contains("2 periods"), "{two}");
        let drm = one.replace("<Representation", r#"<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><Representation"#);
        assert!(matches!(read(&format!("<MPD>{drm}</MPD>"), AT), Err(Error::Protected(_))));
        // What a refused manifest names is still known, for all its periods.
        assert_eq!(named(&format!("<MPD>{drm}{one}</MPD>"), AT).len(), 2);
        for bad in [
            "<html><body>no</body></html>",
            "not xml",
            "<MPD><Period/></MPD>",
            // Neither pieces nor a file.
            r#"<MPD><Period><AdaptationSet mimeType="video/mp4"><Representation id="v"/></AdaptationSet></Period></MPD>"#,
            // A template without a length to count pieces from.
            r#"<MPD><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate duration="4" media="$Number$.m4s"/><Representation id="v"/></AdaptationSet></Period></MPD>"#,
            r#"<MPD mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate duration="4" media="$Sub$.m4s"/><Representation id="v"/></AdaptationSet></Period></MPD>"#,
            r#"<MPD mediaPresentationDuration="P1000Y"><Period><AdaptationSet mimeType="video/mp4"><SegmentTemplate duration="1" media="$Number$.m4s"/><Representation id="v"/></AdaptationSet></Period></MPD>"#,
        ] {
            assert!(matches!(read(bad, AT), Err(Error::Playlist(_))), "{bad}");
        }
    }

    /// Text a page sends can be anything: none of this may take the program down or fill
    /// its memory.
    #[test]
    fn hostile_manifests_are_refused() {
        let deep = format!("<MPD>{}{}</MPD>", "<a>".repeat(5000), "</a>".repeat(5000));
        assert!(matches!(read(&deep, AT), Err(Error::Playlist(_))) && named(&deep, AT).is_empty());
        // Nesting hidden from a careless count: closing tags inside a comment and a value.
        let hidden = format!(r#"<MPD>{}<!-- {} --><b x="{}"/>{}</MPD>"#, "<a>".repeat(40), "</a>".repeat(40), "</a>".repeat(40), "<a>".repeat(40));
        assert!(depth(&hidden) > MAX_DEPTH);
        assert!(depth(NUMBERED) < 8);

        let set = |template: &str, n: usize| {
            let reps: String = (0..n).map(|i| format!(r#"<Representation id="r{i}"/>"#)).collect();
            format!(r#"<MPD mediaPresentationDuration="PT10S"><Period><AdaptationSet mimeType="video/mp4">{template}{reps}</AdaptationSet></Period></MPD>"#)
        };
        let refused = |text: &str| assert!(matches!(read(text, AT), Err(Error::Playlist(_))), "{text}");
        // A repeat to the end of a period that is endless on the template's clock.
        refused(&set(r#"<SegmentTemplate timescale="1e30" media="$Number$.m4s"><SegmentTimeline><S d="1" r="-1"/></SegmentTimeline></SegmentTemplate>"#, 1));
        refused(&set(r#"<SegmentTemplate media="$Number$.m4s"><SegmentTimeline><S d="1" r="9223372036854775806"/></SegmentTimeline></SegmentTemplate>"#, 1));
        // Numbers at the edge of what can be counted: read or refused, no overflow.
        let _ = read(&set(r#"<SegmentTemplate media="$Number$-$Time$.m4s" startNumber="1e30"><SegmentTimeline><S t="9223372036854775807" d="9223372036854775807" r="2"/></SegmentTimeline></SegmentTemplate>"#, 1), AT);
        let _ = read(&set(r#"<SegmentTemplate duration="4" media="$Number$.m4s" startNumber="1e30"/>"#, 1), AT);
        refused(&set(r#"<SegmentTemplate duration="4" media="$Number$.m4s" endNumber="18446744073709551615"/>"#, 1));
        refused(&set(r#"<SegmentList><SegmentURL mediaRange="0-18446744073709551615"/></SegmentList>"#, 1));
        refused(&set(r#"<SegmentList><Initialization sourceURL="i.mp4"/></SegmentList>"#, 1));
        // A width no number has.
        refused(&set(r#"<SegmentTemplate duration="4" media="$Number%099999d$.m4s"/>"#, 1));
        // The limit is the manifest's, not each stream's: 60 000 pieces four times over.
        let many = |n: usize| set(r#"<SegmentTemplate duration="1" media="$RepresentationID$-$Number$.m4s"/>"#, n).replace("PT10S", "PT60000S");
        assert_eq!(read(&many(3), AT).unwrap().streams.len(), 3);
        refused(&many(4));
        assert_eq!(named(&many(4), AT).len(), 180_000);
    }

    #[test]
    fn lengths_as_written() {
        assert_eq!(seconds("PT10M34.5S"), Some(634.5));
        assert_eq!(seconds("P1DT2H"), Some(93_600.0));
        assert_eq!(seconds("P0Y0M0DT0H3M30.000S"), Some(210.0));
        assert_eq!((seconds("10"), seconds("PT5"), seconds("PT1X")), (None, None, None));
    }
}
