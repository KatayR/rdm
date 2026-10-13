// Runs in the page itself (world MAIN), next to its player. Players that load their
// playlist under an address or type that says nothing (/v/a1b2, text/plain, an image type,
// a reply a service worker made) are missed by catch.js, which sees only addresses and
// headers. Here the start of each reply the page fetches is read: one that begins as an
// HLS or DASH playlist is reported to video.js, which hands it to catch.js, with its whole
// text, so RDM need not ask for it again: some addresses answer once, or only to a POST,
// or only to the page. A playlist the page builds itself
// and hands its player as a blob: address is caught where that address is made. Replies
// are read from a copy; the page's own copy is never touched.
(() => {
  // Playlists are small; larger replies, and media by type, are not looked at.
  const MAX_PLAYLIST = 4 * 1024 * 1024;
  // A playlist's text is sent along up to this length; a longer one goes by its address.
  const MAX_TEXT = 2 * 1024 * 1024;
  const SKIP_TYPES = /^(video|audio|font)\/|^text\/(css|javascript)$|javascript|wasm/i;
  const HEAD = 512;
  // Addresses reported, each with a mark of the text it gave ("" without a text, LIVE for
  // a live playlist).
  const reported = new Map();
  const LIVE = "live";

  function kindOf(head) {
    const text = head.replace(/^\uFEFF/, "").trimStart();
    if (text.startsWith("#EXTM3U")) return "hls";
    if (/^(<\?xml[^>]*>\s*)?(<!--[\s\S]*?-->\s*)*<MPD[\s>]/.test(text)) return "dash";
    return null;
  }

  // `text`: the whole playlist, or null when it was too long to keep. One the page built
  // (a blob: address) is only worth reporting with its text. An address is reported once,
  // and again when it later gives another text: some sites serve every video's playlist
  // from one address. A live playlist changes all the time and nothing can be listed from
  // its text: it is reported once, by its address.
  function report(url, kind, text) {
    if (text === null || text.length > MAX_TEXT) text = null;
    const mark = text === null ? "" : isLive(kind, text) ? LIVE : markOf(text);
    if (mark === LIVE) text = null;
    if (/^blob:/i.test(url) ? !text : !/^https?:/i.test(url)) return;
    if (reported.has(url) && (mark === "" || reported.get(url) === mark)) return;
    reported.set(url, mark);
    window.postMessage({ rdmPlaylist: { url, kind, text } }, "*");
  }

  function isLive(kind, text) {
    if (kind === "dash") return /<MPD\b[^>]*\stype\s*=\s*["']dynamic["']/.test(text.slice(0, 4096));
    return !text.includes("#EXT-X-ENDLIST") && !text.includes("#EXT-X-STREAM-INF");
  }

  // Enough to tell one text from another.
  function markOf(text) {
    let sum = 0;
    for (let i = 0; i < text.length; i++) sum = (sum * 31 + text.charCodeAt(i)) | 0;
    return `${text.length}:${sum}`;
  }

  // True when another reply from `url` is not worth reading to its end: a live playlist,
  // or one that was too long to keep (a player may ask for either every few seconds).
  function settled(url) {
    const mark = reported.get(url);
    return mark === LIVE || mark === "";
  }

  // Media by type is not read, except HLS playlists, whose usual types are named as audio.
  function skipped(type) {
    return SKIP_TYPES.test(type || "") && !/mpegurl/i.test(type);
  }

  function worthReading(type, length) {
    return !skipped(type) && !(Number(length) > MAX_PLAYLIST);
  }

  function headOf(bytes) {
    return new TextDecoder().decode(bytes.subarray(0, HEAD));
  }

  const realFetch = window.fetch;
  if (realFetch) {
    window.fetch = function (...args) {
      const answer = realFetch.apply(this, args);
      // Registered before the page's own handlers, so the copy is made before it reads.
      answer.then(sniffResponse, () => {});
      return answer;
    };
  }

  async function sniffResponse(response) {
    try {
      const h = response.headers;
      if (!worthReading(h.get("content-type"), h.get("content-length")) || !response.body) return;
      const reader = response.clone().body.getReader();
      const { value } = await reader.read();
      const kind = value ? kindOf(headOf(value)) : null;
      if (!kind || settled(response.url)) {
        reader.cancel().catch(() => {});
        return;
      }
      report(response.url, kind, await restOf(reader, value));
    } catch {
      // An opaque or already used reply: nothing to read.
    }
  }

  // The reply as text, `first` being what was read of it already; null past MAX_TEXT.
  async function restOf(reader, first) {
    const chunks = [first];
    let size = first.length;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return new Blob(chunks).text();
      size += value.length;
      if (size > MAX_TEXT) {
        reader.cancel().catch(() => {});
        return null;
      }
      chunks.push(value);
    }
  }

  const realSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (...args) {
    this.addEventListener("load", sniffXhr);
    return realSend.apply(this, args);
  };

  function sniffXhr() {
    try {
      if (!worthReading(this.getResponseHeader("content-type"), this.getResponseHeader("content-length"))) return;
      if (this.responseType === "" || this.responseType === "text") {
        const text = this.responseText;
        const kind = kindOf(text.slice(0, HEAD));
        if (kind) report(this.responseURL, kind, text);
      } else if (this.responseType === "arraybuffer" && this.response) {
        const bytes = new Uint8Array(this.response);
        const kind = kindOf(headOf(bytes));
        if (kind && !settled(this.responseURL)) {
          report(this.responseURL, kind, bytes.length <= MAX_TEXT ? new TextDecoder().decode(bytes) : null);
        }
      }
    } catch {
      // Headers or body not readable (cross-origin without CORS): nothing to report.
    }
  }

  // YouTube hands its player the list of a video's formats inside the page. It is read
  // from the player (sizes and codecs; no addresses, nothing else of the page's data), so
  // RDM can show the quality list at once instead of asking YouTube from outside first.
  if (/(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname)) {
    let sent = "";
    const number = (v) => (v === null || v === undefined || v === "" || !Number.isFinite(Number(v)) ? null : Number(v));
    const text = (v, max) => (typeof v === "string" ? v.slice(0, max) : null);
    const readPlayer = () => {
      try {
        // The watch page's player and the one for shorts; of their answers the one for the
        // video this address is about.
        const players = ["movie_player", "shorts-player"].map((id) => document.getElementById(id)).filter((p) => typeof p?.getPlayerResponse === "function");
        const answers = [...players.map((p) => p.getPlayerResponse()), window.ytInitialPlayerResponse].filter((d) => typeof d?.videoDetails?.videoId === "string" && d.streamingData);
        const data = answers.find((d) => location.href.includes(d.videoDetails.videoId)) || answers[0];
        if (!data) return;
        const details = data.videoDetails;
        const streams = data.streamingData;
        const listed = [...(streams.formats || []), ...(streams.adaptiveFormats || [])].filter((f) => Number.isInteger(f?.itag) && typeof f.mimeType === "string");
        const mark = `${details.videoId}:${listed.length}`;
        if (!listed.length || mark === sent) return;
        sent = mark;
        const formats = listed
          .slice(0, 200)
          .map((f) => ({
            itag: f.itag,
            mime: f.mimeType.slice(0, 120),
            width: number(f.width),
            height: number(f.height),
            fps: number(f.fps),
            bitrate: number(f.averageBitrate ?? f.bitrate),
            size: number(f.contentLength),
            quality: text(f.qualityLabel, 40),
            drc: f.isDrc === true,
            track: text(f.audioTrack?.displayName, 80),
            original: /original/i.test(f.audioTrack?.displayName || ""),
            preferred: f.audioTrack?.audioIsDefault === true,
            drm: Array.isArray(f.drmFamilies) && f.drmFamilies.length > 0,
          }));
        const found = { id: details.videoId.slice(0, 40), title: text(details.title, 300), seconds: number(details.lengthSeconds), live: details.isLive === true, formats };
        window.postMessage({ rdmFormats: found }, "*");
      } catch {
        // The player is not there yet, or is another one: yt-dlp lists the page as before.
      }
    };
    // When a video's data is in (also after the site moved on to another video).
    for (const type of ["loadedmetadata", "playing", "yt-navigate-finish"]) document.addEventListener(type, readPlayer, true);
  }

  // A playlist the page put together (from its own script, a JSON reply, a worker's
  // message) reaches the player as a blob: address.
  const realCreate = URL.createObjectURL;
  URL.createObjectURL = function (...args) {
    const url = realCreate.apply(this, args);
    try {
      const made = args[0];
      if (made instanceof Blob && made.size <= MAX_TEXT && !skipped(made.type)) sniffBlob(url, made);
    } catch {
      // Whatever the page did to Blob, it gets its address.
    }
    return url;
  };

  async function sniffBlob(url, blob) {
    try {
      const kind = kindOf(await blob.slice(0, HEAD).text());
      if (kind) report(url, kind, await blob.text());
    } catch {
      // Nothing to read.
    }
  }
})();
