//! Native messaging host: the browser extension's messages, relayed to the daemon.
//!
//! The browser starts `rdm-host` and talks over stdin / stdout, each message a JSON
//! object after its length as a 32-bit native-endian integer. Only the extension named in
//! the host manifest may start it; the browser checks that.

use std::io::{self, Read, Write};
use std::path::Path;

use manager::ipc::Request;
use manager::model::{Caught, NewDownload, Settings, VideoPage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::remote;

/// Chrome's limit for messages from the extension.
const MAX_MESSAGE: usize = 64 << 20;

/// What the extension catches in a tab: file types, skipped hosts, size floors. The one
/// list, built into the extension and into this program.
const RULES: &str = include_str!("../../extension/rules.json");
/// The person's own rules, in the data folder: used instead of the built-in ones.
const OWN_RULES: &str = "catch-rules.json";

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Ping,
    Add {
        url: String,
        referrer: Option<String>,
        cookies: Option<String>,
        user_agent: Option<String>,
        file_name: Option<String>,
        /// Caught from a click on a link that looks like a file; refuse it if the server
        /// answers with a web page, so the browser opens the page instead.
        #[serde(default)]
        from_link: bool,
    },
    /// "Download this video" on a page: the app lists the page's qualities and what the
    /// tab played.
    Video {
        page: String,
        title: Option<String>,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        caught: Vec<Caught>,
        /// What the page's own player was given (`PlayerData`). Taken as it reads: one that
        /// does not read is left out, the window then waits for yt-dlp as before.
        #[serde(default)]
        player: Option<Value>,
    },
    /// The page's video has played a while: list its qualities in the background, so the
    /// button opens with them. Does not start RDM when it is not running.
    ListAhead {
        page: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    /// The extension starts: the rules for what it catches, so a fix to them needs no new
    /// extension. Does not start RDM.
    Rules,
}

/// The next message, or None when the browser closed the pipe.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        other => other?,
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message of {len} bytes")));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

pub fn write_message(w: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    w.write_all(&(body.len() as u32).to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Answers one message. An add is accepted only after the server answered a probe with
/// the browser's cookies, so the extension can leave anything RDM cannot fetch (a POST
/// form, an expired link) to the browser. `home` is the data folder whose manager gets
/// the message.
pub async fn handle(home: &Path, body: &[u8]) -> Value {
    let message = match serde_json::from_slice::<Message>(body) {
        Ok(m) => m,
        Err(e) => return json!({ "ok": false, "error": format!("bad message: {e}") }),
    };
    match message {
        Message::Ping => json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
        Message::Add { url, referrer, cookies, user_agent, file_name, from_link } => {
            let headers: Vec<(String, String)> = [("Referer", referrer), ("Cookie", cookies), ("User-Agent", user_agent)]
                .into_iter()
                .filter_map(|(name, value)| value.filter(|v| !v.is_empty()).map(|v| (name.to_owned(), v)))
                .collect();
            match add(home, url, headers, file_name, from_link).await {
                Ok(Some(id)) => json!({ "ok": true, "id": id }),
                // On the person's list of types the browser keeps: not an error.
                Ok(None) => json!({ "ok": false, "excluded": true }),
                Err(error) => json!({ "ok": false, "error": error }),
            }
        }
        Message::Video { page: url, title, headers, caught, player } => {
            let player = player.and_then(|p| serde_json::from_value(p).ok());
            match choose_video(home, VideoPage { url, title, headers, caught, player }).await {
                Ok(()) => json!({ "ok": true }),
                Err(error) => json!({ "ok": false, "error": error }),
            }
        }
        Message::ListAhead { page, headers } => match list_ahead(home, page, headers).await {
            Ok(()) => json!({ "ok": true }),
            Err(error) => json!({ "ok": false, "error": error }),
        },
        Message::Rules => {
            let (rules, own) = rules(home);
            json!({ "ok": true, "rules": rules, "own": own })
        }
    }
}

/// The catching rules, and whether they are the person's own: `catch-rules.json` in the data
/// folder when it holds a JSON object, else the built-in ones.
fn rules(home: &Path) -> (Value, bool) {
    let own = std::fs::read(home.join(OWN_RULES)).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match own.filter(Value::is_object) {
        Some(own) => (own, true),
        None => (serde_json::from_str(RULES).expect("the built-in rules are JSON"), false),
    }
}

async fn list_ahead(home: &Path, page: String, headers: Vec<(String, String)>) -> Result<(), String> {
    if !(page.starts_with("http://") || page.starts_with("https://")) {
        return Err(format!("only http(s) pages: {page}"));
    }
    let mut client = remote::connect(home, false).await?;
    remote::call(&mut client, &Request::ListAhead { page, headers }).await?;
    Ok(())
}

async fn choose_video(home: &Path, mut page: VideoPage) -> Result<(), String> {
    let web = |url: &str| url.starts_with("http://") || url.starts_with("https://");
    if !web(&page.url) {
        return Err(format!("only http(s) pages: {}", page.url));
    }
    page.caught.retain(usable);
    let mut client = remote::connect(home, true).await?;
    remote::call(&mut client, &Request::ChooseVideo(page)).await?;
    Ok(())
}

/// What the tab played that the program can do something with: an http(s) address, or a
/// playlist the page built itself (a `blob:` address), which has only its text.
fn usable(caught: &Caught) -> bool {
    let web = caught.url.starts_with("http://") || caught.url.starts_with("https://");
    web || (caught.url.starts_with("blob:") && caught.text.is_some())
}

async fn add(
    home: &Path,
    url: String,
    headers: Vec<(String, String)>,
    file_name: Option<String>,
    from_link: bool,
) -> Result<Option<Value>, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("only http(s) links: {url}"));
    }
    let mut client = remote::connect(home, true).await?;
    let settings: Settings = remote::fetch(&mut client, &Request::Settings).await?;
    let name = file_name.clone().unwrap_or_else(|| url.rsplit('/').next().unwrap_or("").to_owned());
    if settings.excludes(name.split('?').next().unwrap_or("")) {
        return Ok(None);
    }
    let probe = engine::probe(&url, &headers).await.map_err(|e| format!("server check failed: {e}"))?;
    if from_link && probe.is_web_page() {
        return Err("the link leads to a web page, not a file".into());
    }
    // With the app running, the person confirms it in the Download File Info window.
    let new = NewDownload { url, headers, file_name, start: true, ask: true, ..NewDownload::default() };
    let added = remote::call(&mut client, &Request::Add(new)).await?;
    Ok(Some(added["id"].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames as the browser writes and reads them, built here by hand: the body's length
    /// as four bytes in the machine's order, then the body.
    #[test]
    fn framing_round_trips() {
        let ping: &[u8] = br#"{"type":"ping"}"#;
        let frame = [&15u32.to_ne_bytes()[..], ping].concat();
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({ "type": "ping" })).unwrap();
        assert_eq!(wire, frame);
        write_message(&mut wire, &json!({ "a": "çok" })).unwrap();
        assert_eq!(wire[frame.len()..], [&12u32.to_ne_bytes()[..], r#"{"a":"çok"}"#.as_bytes()].concat(), "length in bytes");

        let mut r = wire.as_slice();
        assert_eq!(read_message(&mut r).unwrap().unwrap(), ping);
        assert_eq!(serde_json::from_slice::<Value>(&read_message(&mut r).unwrap().unwrap()).unwrap()["a"], "çok");
        assert!(read_message(&mut r).unwrap().is_none(), "end of input");
    }

    #[test]
    fn oversized_and_truncated_messages_fail() {
        // The bytes are all there to read, so only the length it announces refuses a message.
        let announcing = |len: usize| io::Cursor::new((len as u32).to_ne_bytes()).chain(io::repeat(b' '));
        assert!(read_message(&mut announcing(MAX_MESSAGE + 1)).is_err());
        assert_eq!(read_message(&mut announcing(MAX_MESSAGE)).unwrap().unwrap().len(), MAX_MESSAGE, "the longest allowed");
        let mut cut = 10u32.to_ne_bytes().to_vec();
        cut.extend_from_slice(b"{\"a\"");
        assert!(read_message(&mut cut.as_slice()).is_err());
    }

    /// Refused before anything is asked of RDM. The data folder is one of the test's
    /// own all the same: a message wrongly let through must not reach the real one.
    #[tokio::test]
    async fn bad_messages_get_error_replies() {
        let tmp = testserver::Folder::new("host-bad-messages");
        let home = tmp.path();
        assert_eq!(handle(home, br#"{"type":"ping"}"#).await["ok"], true);
        assert_eq!(handle(home, b"not json").await["ok"], false);
        for message in [
            &br#"{"type":"add","url":"ftp://x/y"}"#[..],
            br#"{"type":"video","page":"javascript:alert(1)"}"#,
            br#"{"type":"list_ahead","page":"file:///C:/x.html"}"#,
        ] {
            let reply = handle(home, message).await;
            assert_eq!(reply["ok"], false, "{reply}");
            assert!(reply["error"].as_str().unwrap().contains("only http(s)"), "{reply}");
        }
        assert!(std::fs::read_dir(home).unwrap().next().is_none(), "RDM was started for a refused message");
    }

    /// The extension's rules: the built-in list, until the data folder holds the person's.
    /// Asking does not start RDM.
    #[tokio::test]
    async fn rules_are_the_built_in_ones_or_the_persons() {
        let tmp = testserver::Folder::new("host-rules");
        let home = tmp.path();
        let reply = handle(home, br#"{"type":"rules"}"#).await;
        assert_eq!((&reply["ok"], &reply["own"]), (&json!(true), &json!(false)));
        let built_in: Value = serde_json::from_str(RULES).unwrap();
        assert_eq!(reply["rules"], built_in);
        // What catch.js reads from them.
        for list in ["media_extensions", "audio_extensions", "piece_extensions", "skip_hosts", "playlist_addresses"] {
            assert!(built_in[list].is_array(), "{list}");
        }
        for text in ["hls_types", "dash_types", "piece_types", "media_types"] {
            assert!(built_in[text].is_string(), "{text}");
        }
        for number in ["version", "min_file_size", "min_sound_size", "piece_siblings", "max_caught"] {
            assert!(built_in[number].as_u64().is_some_and(|n| n > 0), "{number}");
        }
        assert!(std::fs::read_dir(home).unwrap().next().is_none(), "RDM was started for the rules");

        for broken in ["not json", "[1, 2]"] {
            std::fs::write(home.join(OWN_RULES), broken).unwrap();
            assert_eq!(handle(home, br#"{"type":"rules"}"#).await["own"], false, "{broken}");
        }
        std::fs::write(home.join(OWN_RULES), r#"{"version":1,"min_file_size":1}"#).unwrap();
        let reply = handle(home, br#"{"type":"rules"}"#).await;
        assert_eq!((&reply["own"], &reply["rules"]["min_file_size"]), (&json!(true), &json!(1)));
    }

    #[test]
    fn list_ahead_message() {
        let body = br#"{"type":"list_ahead","page":"https://www.youtube.com/watch?v=x","headers":[["Cookie","a=b"]]}"#;
        let Message::ListAhead { page, headers } = serde_json::from_slice(body).unwrap() else { panic!() };
        assert_eq!((page.as_str(), headers.len()), ("https://www.youtube.com/watch?v=x", 1));
    }

    /// The keys of each object literal that follows `before` in `source`
    /// (`{ a: 1, b, c: f(x) }` gives a, b, c).
    fn keys_after<'a>(source: &'a str, before: &str) -> Vec<Vec<&'a str>> {
        let literal = |body: &'a str| {
            let (mut depth, mut from, mut keys) = (1, 0, Vec::new());
            for (i, c) in body.char_indices() {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    _ => {}
                }
                if depth == 0 || (depth == 1 && c == ',') {
                    let part = body[from..i].trim();
                    let key = part.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(part, |end| &part[..end]);
                    // (A comment, a spread or a quoted key would need more than this reads.)
                    assert!(!key.is_empty() || part.is_empty(), "not a plain key: {part}");
                    keys.extend((!key.is_empty()).then_some(key));
                    from = i + 1;
                }
                if depth == 0 {
                    break;
                }
            }
            keys
        };
        source.match_indices(before).map(|(at, _)| literal(&source[at + before.len()..])).collect()
    }

    /// The extension's messages, with the keys its own source gives them, are what the host
    /// reads: a key renamed on either side shows here.
    #[test]
    fn the_extension_sends_what_the_host_reads() {
        let background = include_str!("../../extension/background.js");
        let messages = keys_after(background, "sendNativeMessage(HOST, {");
        let scripts = [
            background,
            include_str!("../../extension/catch.js"),
            include_str!("../../extension/content.js"),
            include_str!("../../extension/sniff.js"),
            include_str!("../../extension/types.js"),
            include_str!("../../extension/video.js"),
        ];
        let sends = scripts.iter().map(|script| script.matches("sendNativeMessage(").count()).sum::<usize>();
        assert_eq!(sends, messages.len(), "a message is sent that is not written out where it is sent");
        // What the tab played is put together in catch.js.
        let catch = include_str!("../../extension/catch.js");
        let helper = &catch[catch.find("async function caughtForHelper").expect("caughtForHelper")..];
        let caught = keys_after(helper, "return {").remove(0);
        assert_eq!(caught, ["url", "kind", "size", "headers", "text", "earlier"]);
        let value = |key: &str| match key {
            "type" => unreachable!(),
            "headers" => json!([["Cookie", "a=b"]]),
            "caught" => json!([{ "url": "https://cdn/master.m3u8", "kind": "hls", "size": 7, "headers": [["Referer", "https://player/"]],
                "text": "#EXTM3U", "earlier": true }]),
            "from_link" => json!(true),
            other => json!(format!("https://site/{other}")),
        };
        // Each message's type as its literal starts with it: `{ type: "add", ...`.
        let types = background.split("sendNativeMessage(HOST, {").skip(1).map(|after| {
            let named = after.trim_start().strip_prefix("type: \"").expect("a message begins with its type");
            &named[..named.find('"').unwrap()]
        });
        let mut kinds = Vec::new();
        for (kind, keys) in types.zip(&messages) {
            let read = match kind {
                "add" => &["url", "referrer", "cookies", "user_agent", "file_name", "from_link"][..],
                "video" => &["page", "title", "headers", "caught", "player"],
                "list_ahead" => &["page", "headers"],
                "rules" => &[],
                other => panic!("a message the host does not read: {other}"),
            };
            assert_eq!((keys[0], &keys[1..]), ("type", read), "{kind}");
            let mut message = serde_json::Map::from_iter(keys[1..].iter().map(|k| (k.to_string(), value(k))));
            message.insert("type".into(), json!(kind));
            match serde_json::from_value::<Message>(Value::Object(message)).unwrap() {
                Message::Add { referrer, cookies, user_agent, file_name, from_link, .. } => {
                    assert!(referrer.is_some() && cookies.is_some() && user_agent.is_some() && file_name.is_some() && from_link);
                }
                Message::Video { title, headers, caught, .. } => {
                    assert!(title.is_some() && headers.len() == 1);
                    assert_eq!((caught.len(), caught[0].size, caught[0].headers.len()), (1, Some(7), 1));
                    assert_eq!((caught[0].text.as_deref(), caught[0].earlier), (Some("#EXTM3U"), true));
                }
                Message::ListAhead { headers, .. } => assert_eq!(headers.len(), 1),
                Message::Rules => {}
                Message::Ping => panic!("no ping is sent"),
            }
            kinds.push(kind);
        }
        kinds.sort_unstable();
        assert_eq!(kinds, ["add", "list_ahead", "rules", "video"]);
    }

    /// "Download this video" with what the tab played, as the host reads it.
    #[test]
    fn video_message_with_caught_streams() {
        let body = br#"{"type":"video","page":"https://player/e/1","title":"Episode 1","headers":[["Referer","https://site/ep1"]],"caught":[
            {"url":"https://cdn/master.m3u8","kind":"hls","size":null,"headers":[["Referer","https://player/"],["Cookie","a=b"]]},
            {"url":"https://cdn/v.mp4","kind":"file","size":7340032,"headers":[]}]}"#;
        let Message::Video { page, title, headers, caught, .. } = serde_json::from_slice(body).unwrap() else { panic!() };
        assert_eq!((page.as_str(), title.as_deref(), caught.len()), ("https://player/e/1", Some("Episode 1"), 2));
        assert_eq!(headers, [("Referer".to_owned(), "https://site/ep1".to_owned())]);
        assert_eq!(caught[0].kind, manager::model::CaughtKind::Hls);
        assert_eq!(caught[0].headers[1], ("Cookie".to_owned(), "a=b".to_owned()));
        assert_eq!(caught[1].size, Some(7340032));
    }

    #[test]
    fn caught_entries_without_an_address_need_their_text() {
        let entry = |url: &str, text: Option<&str>| Caught {
            url: url.into(),
            kind: manager::model::CaughtKind::Hls,
            size: None,
            headers: Vec::new(),
            text: text.map(str::to_owned),
            earlier: false,
        };
        assert!(usable(&entry("https://cdn/master.m3u8", None)) && usable(&entry("http://cdn/a", Some("#EXTM3U"))));
        assert!(usable(&entry("blob:https://site/1-2", Some("#EXTM3U"))));
        assert!(!usable(&entry("blob:https://site/1-2", None)), "nothing to ask and nothing to read");
        assert!(!usable(&entry("data:text/plain,x", Some("#EXTM3U"))) && !usable(&entry("file:///C:/x.m3u8", None)));
    }
}
