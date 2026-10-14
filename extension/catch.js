// Video catching, as IDM does it: every media response a tab receives is noted, whatever
// the site, so "Download this video" can offer what the page played even where yt-dlp does
// not know the site. Kept per tab in session storage (the service worker may sleep) and
// forgotten when the tab loads a new page or closes. Read-only: requests are never changed.
//
// What counts as media is in rules.json (types, extensions, skipped hosts, size floors). The
// program sends its own copy when the extension starts (background.js), so a fix to the
// rules needs no new extension.

// A media name does not make a page, a picture or data a media file.
const NOT_MEDIA_TYPES = /^(text|image)\/|json|xml|javascript/i;
// A single-page site that moves on usually asks for its next video a moment before its
// address changes: what was caught this shortly before still belongs to the new page.
const TURN_GRACE_MS = 5000;
// The program's rules are asked for again when the last asking is this old.
const RULES_FOR_MS = 60 * 60 * 1000;
// A playlist's text is kept up to this length (sniff.js sends no longer one; a page could).
const MAX_TEXT = 2 * 1024 * 1024;
// The texts kept for one tab, together. Session storage holds 10 MB for all tabs.
const TAB_TEXTS = 4 * 1024 * 1024;

const CAUGHT_FILTER = {
  urls: ["http://*/*", "https://*/*"],
  types: ["media", "xmlhttprequest", "object", "other", "main_frame", "sub_frame"],
};

// The rules in use, as regular expressions and numbers.
let rules = null;
const rulesReady = loadRules();

// The built-in rules, or the program's stored ones when those are the person's own or at
// least as new.
async function loadRules() {
  const builtIn = await (await fetch(chrome.runtime.getURL("rules.json"))).json();
  rules = compileRules(builtIn);
  const { rules: stored, rulesAsked = 0 } = await chrome.storage.local.get(["rules", "rulesAsked"]);
  if (stored) useRules(stored, builtIn);
  if (Date.now() - rulesAsked > RULES_FOR_MS) refreshRules(builtIn);
}

// Takes the program's rules (`{ rules, own }`) into use. What they leave out is as built
// in (rules written for an earlier version lack what was added since); rules that do not
// read are left aside altogether.
function useRules(given, builtIn) {
  if (!(given.own || given.rules?.version >= builtIn.version)) return;
  try {
    rules = compileRules({ ...builtIn, ...given.rules });
  } catch (e) {
    console.warn("RDM: the program's catching rules do not read:", e);
  }
}

function compileRules(r) {
  for (const [name, value] of Object.entries(r)) {
    const right = /_extensions$|^skip_hosts$|^playlist_addresses$/.test(name)
      ? Array.isArray(value)
      : /_types$/.test(name)
        ? typeof value === "string"
        : /^(version|min_\w+|piece_siblings|max_caught)$/.test(name)
          ? Number.isFinite(value) && value > 0
          : true; // a name these rules do not know
    if (!right) throw new Error(`"${name}" is not as expected`);
  }
  const named = (extensions) => new RegExp(`\\.(${extensions.join("|")})$`, "i");
  return {
    version: r.version,
    // Plain media files, and which of them are sound.
    media: named(r.media_extensions),
    audio: named(r.audio_extensions),
    // Pieces of a playlist's stream: never offered alone.
    piece: named(r.piece_extensions),
    mediaTypes: new RegExp(r.media_types, "i"),
    hlsTypes: new RegExp(r.hls_types, "i"),
    dashTypes: new RegExp(r.dash_types, "i"),
    pieceTypes: new RegExp(r.piece_types, "i"),
    // YouTube's streams are listed by yt-dlp from the page; tiny clips and stickers
    // (gstatic, tenor, giphy) are not caught.
    skipHosts: r.skip_hosts.map((host) => new RegExp(host, "i")),
    // Addresses to take as a playlist whatever their name and type say.
    playlists: r.playlist_addresses.map((p) => ({ address: new RegExp(p.address, "i"), kind: p.kind })),
    // What a script fetched and is smaller than this is a preview, a sound or an ad.
    minFile: r.min_file_size,
    // What a media element played and is smaller than this is an interface sound.
    minSound: r.min_sound_size,
    // Different addresses that differ only in their numbers, seen this often in one tab,
    // are the pieces of a stream (seg-1.mp4, seg-2.mp4 ...).
    siblings: r.piece_siblings,
    max: r.max_caught,
  };
}

// Storage changes one at a time, so two responses arriving together cannot drop each other.
let storageQueue = Promise.resolve();
function serially(work) {
  storageQueue = storageQueue.then(work).catch((e) => console.warn("RDM:", e));
  return storageQueue;
}

// Referer and Origin as the browser sent them, until the response arrives.
const sentHeaders = new Map();
// Per tab: address patterns seen for media files, and the addresses seen for each.
const siblings = new Map();
// Per tab: Referer and Origin of its latest requests by address, for what sniff.js finds.
const recentSent = new Map();
const RECENT_SENT = 300;
// Per tab: the address of each frame's page, to tell what the clicked video's frame loaded.
// Lost when the service worker sleeps; the list is then only not ordered by frame.
const frameUrls = new Map();

function header(headers, name) {
  return headers?.find((h) => h.name.toLowerCase() === name)?.value;
}

function pathOf(url) {
  try {
    return new URL(url).pathname;
  } catch {
    return "";
  }
}

function withoutFragment(url) {
  return url.split("#")[0];
}

// The file name the server gives in Content-Disposition, or "".
function givenName(headers) {
  const value = header(headers, "content-disposition");
  if (!value) return "";
  const encoded = /filename\*\s*=\s*[^']*'[^']*'([^;]+)/i.exec(value);
  if (encoded) {
    try {
      return decodeURIComponent(encoded[1].trim());
    } catch {
      // Not as the standard writes it: the plain name is tried.
    }
  }
  const plain = /filename\s*=\s*(?:"([^"]*)"|([^;]+))/i.exec(value);
  return plain ? (plain[1] ?? plain[2]).trim() : "";
}

// "hls", "dash", "file" or null. Judged as IDM does: by the name the server gives the
// file, else the name in the address, and only then by the type.
function mediaKind(url, type, name) {
  const listed = rules.playlists.find((p) => p.address.test(url));
  if (listed) return listed.kind;
  const path = nameOf(url, name);
  if (/\.m3u8$/i.test(path) || rules.hlsTypes.test(type)) return "hls";
  if (/\.mpd$/i.test(path) || rules.dashTypes.test(type)) return "dash";
  if (rules.piece.test(path) || rules.pieceTypes.test(type)) return null;
  if (rules.media.test(path) && !NOT_MEDIA_TYPES.test(type)) return "file";
  return rules.mediaTypes.test(type) ? "file" : null;
}

// The name to judge a reply by: the one the server gives when it has an extension, else
// the path of the address.
function nameOf(url, name) {
  return /\.\w{1,5}$/.test(name) ? name : pathOf(url);
}

// What a media element asked for is what the person plays, however short; only interface
// sounds are left out. What a script fetched has to be large to count.
function tooSmall(details, size, type, name) {
  if (size === null) return false;
  if (details.type !== "media") return size < rules.minFile;
  const sound = /^audio\//i.test(type) || rules.audio.test(nameOf(details.url, name));
  return sound && size < rules.minSound;
}

// Total size from Content-Range (206) or Content-Length (200).
function totalSize(details) {
  const range = header(details.responseHeaders, "content-range");
  const total = range && /\/(\d+)\s*$/.exec(range);
  if (total) return Number(total[1]);
  const length = header(details.responseHeaders, "content-length");
  return details.statusCode === 200 && length ? Number(length) : null;
}

function caughtKey(tabId) {
  return `caught:${tabId}`;
}

// The tab's page as last seen, and when a single-page site last moved on.
function pageKey(tabId) {
  return `page:${tabId}`;
}

async function caughtIn(tabId) {
  const key = caughtKey(tabId);
  return (await chrome.storage.session.get(key))[key] || [];
}

async function pageIn(tabId) {
  const key = pageKey(tabId);
  return (await chrome.storage.session.get(key))[key] || { url: null, turned: 0 };
}

// Saves a tab's list. Its texts together stay within TAB_TEXTS, and when session storage
// is full all the same (other tabs' texts), the oldest texts go one by one until it fits.
async function keep(tabId, items) {
  const key = caughtKey(tabId);
  let size = items.reduce((sum, i) => sum + (i.text?.length || 0), 0);
  for (;;) {
    if (size <= TAB_TEXTS) {
      try {
        await chrome.storage.session.set({ [key]: items });
        break;
      } catch (e) {
        if (!items.some((i) => i.text)) throw e;
      }
    }
    // By when the tab last asked for it, not by its place in the list.
    const oldest = items.filter((i) => i.text).reduce((a, b) => (b.at < a.at ? b : a));
    size -= oldest.text.length;
    items = withoutText(items, oldest);
  }
}

// `items` without `entry`'s text. A playlist the page built is nothing without its text
// and goes altogether; one with an address stays to be asked by it.
function withoutText(items, entry) {
  if (/^blob:/i.test(entry.url)) return items.filter((i) => i !== entry);
  return items.map((i) => (i === entry ? { ...i, text: null } : i));
}

// True when `url` is the third (or later) numbered sibling of earlier media files in the
// tab: the whole family is then a stream's pieces and is dropped.
function isPiece(tabId, url) {
  const pattern = url.split("?")[0].replace(/\d+/g, "#");
  if (!siblings.has(tabId)) siblings.set(tabId, new Map());
  const seen = siblings.get(tabId);
  const urls = seen.get(pattern) || new Set();
  urls.add(url.split("?")[0]);
  seen.set(pattern, urls);
  return urls.size >= rules.siblings ? pattern : null;
}

async function note(details) {
  const sent = sentHeaders.get(details.requestId);
  sentHeaders.delete(details.requestId);
  if (details.tabId < 0 || !(details.statusCode === 200 || details.statusCode === 206)) return;
  await rulesReady;
  const url = details.url;
  let host;
  try {
    host = new URL(url).hostname;
  } catch {
    return;
  }
  if (rules.skipHosts.some((skipped) => skipped.test(host))) return;
  const type = (header(details.responseHeaders, "content-type") || "").split(";")[0].trim();
  const name = givenName(details.responseHeaders);
  const kind = mediaKind(url, type, name);
  if (!kind) return;
  const size = totalSize(details);
  if (kind === "file" && tooSmall(details, size, type, name)) return;

  const tabId = details.tabId;
  const pieces = kind === "file" ? isPiece(tabId, url) : null;
  if (pieces) {
    const items = await caughtIn(tabId);
    const kept = items.filter((i) => i.url.split("?")[0].replace(/\d+/g, "#") !== pieces);
    if (kept.length !== items.length) await keep(tabId, kept);
    return;
  }
  await addCaught(tabId, {
    url,
    kind,
    type,
    size,
    referer: sent?.referer || null,
    origin: sent?.origin || null,
    frameId: details.frameId,
  });
}

// Adds what a tab received. An address already noted takes the text the page read of it,
// also a new one (a site may serve every video's playlist from one address); and one
// marked as earlier counts as now again, since the tab asked for it again.
async function addCaught(tabId, item) {
  const items = await caughtIn(tabId);
  const known = items.find((i) => i.url === item.url) || items.find((i) => sameFile(i, item));
  if (!known) {
    await keep(tabId, [...items, { ...item, at: Date.now() }].slice(-rules.max));
    return;
  }
  const { turned } = await pageIn(tabId);
  const newText = !!item.text && item.text !== known.text;
  if (!newText && known.at >= turned) return;
  if (newText) Object.assign(known, { text: item.text, kind: item.kind });
  known.at = Date.now();
  await keep(tabId, items);
}

// True for one file met under two addresses: the same name and the same size to the byte.
// Players read a file from mirrors, or with a counter in the address; it is offered once.
function sameFile(a, b) {
  const name = (url) => pathOf(url).split("/").pop();
  return a.kind === "file" && b.kind === "file" && !!a.size && a.size === b.size && name(a.url) === name(b.url);
}

// A playlist sniff.js recognised by its content in a frame of the tab (`frame`: that
// frame's address, origin and id), with its text. Sent with the headers the
// browser used for it when webRequest saw the request, else with the ones the frame would
// send. One the page built itself (a blob: address) cannot be asked for later: only its
// text makes it worth noting.
function noteSniffed(tabId, found, frame) {
  if (!(found.kind === "hls" || found.kind === "dash")) return;
  const given = typeof found.text === "string" && found.text.length <= MAX_TEXT;
  const text = given ? found.text : null;
  const built = /^blob:/i.test(found.url);
  if (built ? !text : !/^https?:/i.test(found.url)) return;
  const sent = recentSent.get(tabId)?.get(found.url);
  const origin = frame.origin && frame.origin !== "null" ? frame.origin : null;
  serially(async () => {
    await rulesReady;
    await addCaught(tabId, {
      url: found.url,
      kind: found.kind,
      type: null,
      size: null,
      text,
      referer: sent ? sent.referer || null : frame.url || null,
      origin: sent ? sent.origin || null : built || crossOrigin(found.url, origin) ? origin : null,
      frameId: frame.frameId,
    });
  });
}

function crossOrigin(url, origin) {
  try {
    return !!origin && new URL(url).origin !== origin;
  } catch {
    return false;
  }
}

// The tab loads a new page at `url`, or closes: its list starts over.
async function forgetTab(tabId, url = null) {
  siblings.delete(tabId);
  recentSent.delete(tabId);
  frameUrls.delete(tabId);
  await chrome.storage.session.remove([caughtKey(tabId), pageKey(tabId)]);
  if (url) await chrome.storage.session.set({ [pageKey(tabId)]: { url: withoutFragment(url), turned: 0 } });
}

// The tab's address changed. Without a page load (a single-page site moved on to another
// video) what was caught before is probably the last video's: it is kept, since a site may
// also rewrite its address while one video plays, but marked as earlier.
async function noteAddress(tabId, url) {
  const page = await pageIn(tabId);
  const now = withoutFragment(url);
  if (page.url === now) return;
  const turned = page.url ? Date.now() - TURN_GRACE_MS : page.turned;
  await chrome.storage.session.set({ [pageKey(tabId)]: { url: now, turned } });
  frameUrls.get(tabId)?.set(0, now);
}

// `items` with `earlier` set on what was caught before the tab's site last moved on. When
// that is everything, nothing is marked: the address changed under one video.
function withEarlier(items, turned) {
  const all = items.every((i) => i.at < turned);
  return items.map((i) => ({ ...i, earlier: !all && i.at < turned }));
}

chrome.webRequest.onSendHeaders.addListener(
  (details) => {
    if (details.tabId < 0) return;
    const referer = header(details.requestHeaders, "referer");
    const origin = header(details.requestHeaders, "origin");
    if (!(referer || origin)) return;
    sentHeaders.set(details.requestId, { referer, origin });
    if (!recentSent.has(details.tabId)) recentSent.set(details.tabId, new Map());
    const recent = recentSent.get(details.tabId);
    recent.delete(details.url);
    recent.set(details.url, { referer, origin });
    if (recent.size > RECENT_SENT) recent.delete(recent.keys().next().value);
  },
  CAUGHT_FILTER,
  ["requestHeaders", "extraHeaders"],
);

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    // A new page in the tab starts a new list; a video opened directly in the tab is noted
    // after that.
    if (details.type === "main_frame") serially(() => forgetTab(details.tabId, details.url));
    if (details.tabId >= 0 && (details.type === "main_frame" || details.type === "sub_frame")) {
      serially(() => {
        if (!frameUrls.has(details.tabId)) frameUrls.set(details.tabId, new Map());
        frameUrls.get(details.tabId).set(details.frameId, withoutFragment(details.url));
      });
    }
    serially(() => note(details));
  },
  CAUGHT_FILTER,
  ["responseHeaders"],
);

for (const event of [chrome.webRequest.onCompleted, chrome.webRequest.onErrorOccurred]) {
  event.addListener((details) => sentHeaders.delete(details.requestId), CAUGHT_FILTER);
}

chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.url) serially(() => noteAddress(tabId, change.url));
});

chrome.tabs.onRemoved.addListener((tabId) => serially(() => forgetTab(tabId)));

// What a tab played, as the helper takes it: address, kind, size, the headers to fetch it
// with (the browser's Referer and Origin, its cookies for that address, its user agent),
// a playlist's text when the page read it, and whether it is from before the tab's
// site last moved on. First the file the clicked video plays (`playing`), then what the
// frame showing `page` loaded, then the rest.
async function caughtForHelper(tabId, page, playing) {
  const { turned } = await pageIn(tabId);
  const items = withEarlier(await caughtIn(tabId), turned);
  const frames = new Set();
  for (const [frameId, url] of frameUrls.get(tabId) || []) {
    if (url === withoutFragment(page)) frames.add(frameId);
  }
  const played = playing ? withoutFragment(playing) : null;
  const place = (i) => (withoutFragment(i.url) === played ? 0 : frames.has(i.frameId) ? 1 : 2);
  // (The sort keeps the order of what it finds equal.)
  items.sort((a, b) => place(a) - place(b));
  return Promise.all(
    items.map(async (i) => {
      const headers = [["User-Agent", navigator.userAgent]];
      if (i.referer) headers.push(["Referer", i.referer]);
      if (i.origin) headers.push(["Origin", i.origin]);
      const cookies = /^https?:/i.test(i.url) ? await cookiesFor(i.url) : "";
      if (cookies) headers.push(["Cookie", cookies]);
      return { url: i.url, kind: i.kind, size: i.size, headers, text: i.text || null, earlier: i.earlier };
    }),
  );
}
