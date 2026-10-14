// RDM browser capture.
//
// Every download the browser starts goes to RDM, unless its file type is on the
// person's exclusion list (kept in RDM's settings, checked by the helper), it was
// Alt+clicked, or capture is switched off.
// The download is held at the file-name step while RDM checks the link with the
// browser's cookies. Only when RDM takes it is the browser download cancelled; anything
// else lets the browser carry on, so no download is lost.

importScripts("types.js", "catch.js");

const HOST = "com.rdm.host";

// Longest wait for RDM's answer before the browser keeps the download.
const ANSWER_WITHIN_MS = 30000;

// Links Alt+clicked in a page, let through to the browser for a short while.
const SKIP_FOR_MS = 15000;
const skipped = new Map();

function baseName(path) {
  return path.split(/[\\/]/).pop();
}

function takeSkip(url) {
  const at = skipped.get(url);
  skipped.delete(url);
  return at !== undefined && Date.now() - at < SKIP_FOR_MS;
}

async function captureOn() {
  const { capture = true } = await chrome.storage.local.get("capture");
  return capture;
}


async function cookiesFor(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

// Downloads and videos on their way to RDM: `sent` is a message to the helper, counted
// until its answer. While one is, the extension does not reload itself (below): the
// download would end up in the browser and in RDM, or in neither.
let handingOver = 0;
function handOver(sent) {
  handingOver++;
  const over = () => handingOver--;
  sent.then(over, over);
  return sent;
}

// RDM writes the extension's files anew when it is installed or updated, but the browser
// goes on running the copy it loaded: an unpacked extension is read from its folder only
// when it is loaded. The manifest there is read at any time, though: another version in it
// than the one running means newer files. (A copy from a store is always its own folder's.)
async function replacedOnDisk() {
  try {
    const onDisk = await (await fetch(chrome.runtime.getURL("manifest.json"), { cache: "no-store" })).json();
    return typeof onDisk.version === "string" && onDisk.version !== chrome.runtime.getManifest().version;
  } catch {
    return false;
  }
}

// Loads the newer files, once nothing is on its way to RDM. Looked at each time the
// browser wakes the extension, a moment later: what woke it comes first.
async function reloadIfReplaced() {
  if (!(await replacedOnDisk())) return;
  const whenIdle = () => (handingOver === 0 ? chrome.runtime.reload() : setTimeout(whenIdle, 1000));
  whenIdle();
}
setTimeout(reloadIfReplaced, 3000);

// Resolves with the new download's id, or throws with RDM's reason. `fromLink`: a
// clicked link that only looks like a file; RDM refuses it if it is a web page.
async function send(url, referrer, fileName, fromLink = false) {
  const message = chrome.runtime.sendNativeMessage(HOST, {
    type: "add",
    url,
    referrer: referrer || null,
    cookies: (await cookiesFor(url)) || null,
    user_agent: navigator.userAgent,
    file_name: fileName || null,
    from_link: fromLink,
  });
  const asked = handOver(message);
  const late = new Promise((_, reject) =>
    setTimeout(() => reject(new Error("RDM did not answer in time")), ANSWER_WITHIN_MS),
  );
  const reply = await Promise.race([asked, late]);
  if (reply?.excluded) throw Object.assign(new Error("excluded"), { excluded: true });
  if (!reply || !reply.ok) throw new Error(reply ? reply.error : "no reply from RDM");
  return reply.id;
}

// The button's state: "off" while capture is off, else nothing.
async function showState(on) {
  await chrome.action.setBadgeText({ text: on ? "" : "off" });
  await chrome.action.setTitle({
    title: on ? "RDM: capturing downloads (click to turn off)" : "RDM: capture is off (click to turn on)",
  });
}

// Shown for a while.
async function showError(message) {
  console.warn("RDM:", message);
  await chrome.action.setBadgeText({ text: "!" });
  await chrome.action.setTitle({ title: "RDM: " + message });
  setTimeout(async () => showState(await captureOn()), 10000);
}

// The program's rules for what catch.js catches, asked for when the extension starts and
// when the stored ones are old. Without an answer (no RDM, or an older one) the ones in
// use stay. RDM is not started for it.
async function refreshRules(builtIn) {
  try {
    await chrome.storage.local.set({ rulesAsked: Date.now() });
    const reply = await chrome.runtime.sendNativeMessage(HOST, { type: "rules" });
    if (!reply?.ok || typeof reply.rules !== "object") return;
    const given = { rules: reply.rules, own: reply.own === true };
    await chrome.storage.local.set({ rules: given });
    useRules(given, builtIn);
  } catch {
    // The built-in rules do.
  }
}

// True when RDM took the download.
async function capture(item) {
  // The address after redirects: often a file server past the site's bot check, which
  // the original link would lead RDM straight back into.
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url) || item.byExtensionId === chrome.runtime.id) return false;
  if (takeSkip(item.url) || takeSkip(item.finalUrl)) return false;
  const fileName = baseName(item.filename);
  if (!(await captureOn())) return false;
  try {
    await send(url, item.referrer, fileName);
  } catch (e) {
    if (!e.excluded) await showError(`left to the browser: ${e.message}`);
    return false;
  }
  await chrome.downloads.cancel(item.id);
  await chrome.downloads.erase({ id: item.id });
  return true;
}

chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  capture(item).then(
    (taken) => taken || suggest(),
    (e) => {
      console.error("RDM:", e);
      suggest();
    },
  );
  return true; // suggest() is called later
});

// From content.js: Alt+clicked links to let through, and clicked links to take. From
// video.js: "Download this video", and playlists sniff.js found in the page.
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (typeof message?.skip === "string") {
    skipped.set(message.skip, Date.now());
    return false;
  }
  if (typeof message?.playlist?.url === "string") {
    if (sender.tab) {
      noteSniffed(sender.tab.id, message.playlist, { url: sender.url, origin: sender.origin, frameId: sender.frameId });
    }
    return false;
  }
  if (typeof message?.video === "string") {
    chooseVideo(message.video, sender.tab, typeof message.playing === "string" ? message.playing : null).then(sendResponse);
    return true; // answered later
  }
  if (message?.formats && sender.tab) {
    const found = youtubeHost(sender.url) ? playerData(message.formats) : null;
    if (found) serially(() => notePlayer(sender.tab.id, found));
    return false;
  }
  if (typeof message?.ahead === "string") {
    listAhead(message.ahead, sender.tab);
    return false;
  }
  if (typeof message?.take !== "string") return false;
  takeLink(message.take, message.referrer).then((taken) => sendResponse({ taken }));
  return true; // answered later
});

async function takeLink(url, referrer) {
  if (!(await captureOn())) {
    skipped.set(url, Date.now());
    return false;
  }
  try {
    await send(url, referrer, null, true);
    return true;
  } catch (e) {
    // The click is replayed in the page; let the browser keep that download.
    skipped.set(url, Date.now());
    if (!e.excluded) await showError(`left to the browser: ${e.message}`);
    return false;
  }
}

// The app opens its Download Video window for the page (or the frame holding the player),
// with what the tab played (catch.js), the file the clicked video plays (`playing`) first.
async function chooseVideo(page, tab, playing) {
  try {
    const caught = tab ? await caughtForHelper(tab.id, page, playing) : [];
    // A player in a blank frame has no address of its own; the tab's page stands in.
    if (!/^https?:/i.test(page) && tab?.url) page = tab.url;
    const title = tab?.title || null;
    const headers = await pageHeaders(page, tab);
    const player = tab ? await playerFor(tab.id, page) : null;
    const message = chrome.runtime.sendNativeMessage(HOST, { type: "video", page, title, headers, caught, player });
    const reply = await handOver(message);
    if (!reply?.ok) throw new Error(reply ? reply.error : "no reply from RDM");
    return { ok: true };
  } catch (e) {
    await showError(e.message);
    return { ok: false };
  }
}

// The format lists YouTube's player was given in a tab (sniff.js), the latest few: the
// one of the video a button press is about goes to RDM with it, so the quality list is
// there at once.
const PLAYERS_KEPT = 4;
const playerKey = (tabId) => `player:${tabId}`;

// What a YouTube frame's script sent, taken field by field: the page's script wrote it.
function playerData(sent) {
  const text = (v, max) => (typeof v === "string" ? v.slice(0, max) : null);
  const number = (v) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null);
  if (!/^[\w-]{1,40}$/.test(sent?.id) || !Array.isArray(sent.formats)) return null;
  const formats = sent.formats
    .slice(0, 200)
    .filter((f) => Number.isInteger(f?.itag) && f.itag >= 0 && typeof f.mime === "string")
    .map((f) => ({
      itag: f.itag,
      mime: f.mime.slice(0, 120),
      width: number(f.width),
      height: number(f.height),
      fps: number(f.fps),
      bitrate: number(f.bitrate),
      size: number(f.size),
      quality: text(f.quality, 40),
      drc: f.drc === true,
      track: text(f.track, 80),
      original: f.original === true,
      preferred: f.preferred === true,
      drm: f.drm === true,
    }));
  return { id: sent.id, title: text(sent.title, 300), seconds: number(sent.seconds), live: sent.live === true, formats };
}

function youtubeHost(address) {
  try {
    return /(^|\.)youtube(-nocookie)?\.com$/.test(new URL(address).hostname);
  } catch {
    return false;
  }
}

async function notePlayer(tabId, found) {
  const key = playerKey(tabId);
  const kept = ((await chrome.storage.session.get(key))[key] || []).filter((p) => p.id !== found.id);
  await chrome.storage.session.set({ [key]: [...kept, found].slice(-PLAYERS_KEPT) }).catch(() => {});
}

async function playerFor(tabId, page) {
  const id = youtubeId(page);
  if (!id) return null;
  const key = playerKey(tabId);
  return ((await chrome.storage.session.get(key))[key] || []).find((p) => p.id === id) || null;
}

// The video a YouTube address is about: a watch page, a short, an embedded player.
function youtubeId(page) {
  if (!youtubeHost(page)) return null;
  const url = new URL(page);
  if (url.pathname === "/watch") return url.searchParams.get("v");
  return /^\/(embed|shorts|live|v)\/([\w-]+)/.exec(url.pathname)?.[2] || null;
}

chrome.tabs.onRemoved.addListener((tabId) => chrome.storage.session.remove(playerKey(tabId)).catch(() => {}));

// A video in the tab has played a while (video.js): RDM lists the page's qualities in
// the background. Only a head start for the button, so failures stay quiet, and RDM is
// not started for it.
async function listAhead(page, tab) {
  if (!(await captureOn())) return;
  if (!/^https?:/i.test(page) && tab?.url) page = tab.url;
  try {
    const headers = await pageHeaders(page, tab);
    await chrome.runtime.sendNativeMessage(HOST, { type: "list_ahead", page, headers });
  } catch {
    // The button still works without it.
  }
}

// What the browser sends for the page: a player's frame is fetched with the tab's page as
// Referer (embed pages often refuse anything else), and every page with its cookies.
async function pageHeaders(page, tab) {
  const headers = [["User-Agent", navigator.userAgent]];
  if (tab?.url && tab.url !== page && /^https?:/i.test(tab.url)) headers.push(["Referer", tab.url]);
  const cookies = await cookiesFor(page);
  if (cookies) headers.push(["Cookie", cookies]);
  return headers;
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "rdm-video") {
    await chooseVideo(info.frameUrl || info.pageUrl, tab, info.srcUrl || null);
    return;
  }
  if (info.menuItemId !== "rdm-link") return;
  try {
    await send(info.linkUrl, info.pageUrl, null);
  } catch (e) {
    await showError(e.message);
  }
});

chrome.action.onClicked.addListener(async () => {
  const on = !(await captureOn());
  await chrome.storage.local.set({ capture: on });
  await showState(on);
});

chrome.runtime.onInstalled.addListener(async () => {
  chrome.contextMenus.create({ id: "rdm-link", title: "Download with RDM", contexts: ["link"] });
  // For players that hide their video from the button, and for what the tab played.
  chrome.contextMenus.create({
    id: "rdm-video",
    title: "Download video with RDM",
    contexts: ["page", "frame", "video", "audio"],
  });
  await showState(await captureOn());
});

chrome.runtime.onStartup.addListener(async () => showState(await captureOn()));
