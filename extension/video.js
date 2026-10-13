// "Download this video" over videos playing in a page, as IDM's panel.
//
// While the pointer is over a video (or the button), or while a video plays with sound, a
// small icon button sits just above the video, at its right edge. A click sends this
// frame's address to RDM, which looks up the page's qualities and lets the person choose
// one; the file the video plays goes with it, to be listed first. In an embedded player the
// frame's address is the player's own page (youtube.com/embed/...), which is what RDM
// needs. The button lives in a closed shadow root, so the page's styles cannot reach it.
// Players built in blank or generated frames (about:blank, blob:) get the button too; the
// background then sends the tab's page instead of the frame's address.
//
// A player that fills its frame leaves no room above the video inside that frame, so the
// frame asks the page around it to draw the button above the frame instead (and that page
// its own parent, as far as needed). The top page draws it inside the video's corner when
// even it has no room.

(() => {
  if (!/^(https?|about|blob|data):$/.test(location.protocol)) return;

  // Videos smaller than this are previews and thumbnails, not something to download.
  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 112;
  // The button stays this long after the pointer leaves the video.
  const LINGER_MS = 2500;
  // Space between the button and the video's top edge.
  const GAP = 3;
  const IDLE_TITLE = "Download this video with RDM";

  let enabled = true;
  let host = null;
  let button = null;
  // The video in this frame the button is for, or a child frame's video this frame draws
  // the button for ({ source: its window, rect: the video in its viewport, page, playing:
  // the file it plays, keep: it plays with sound }).
  let current = null;
  let remote = null;
  // The video in this frame that plays with sound: the one being watched. The button stays
  // at it without the pointer, as IDM's panel does.
  let pinned = null;
  // True while an ancestor frame draws the button for `current` or `remote`.
  let forwarded = false;
  let hideTimer = 0;
  // Videos whose button was closed, until the video changes or the page reloads. Keyed by
  // what is playing rather than the element alone: YouTube keeps one <video> element and
  // swaps the video inside it.
  const dismissed = new WeakMap();

  function playing(video) {
    return `${location.href} ${video.currentSrc || video.src}`;
  }

  // The file a video plays, when it plays one (not a stream its player feeds it).
  function fileOf(video) {
    const src = video.currentSrc || video.src;
    return /^https?:/i.test(src) ? src : null;
  }

  function audible(video) {
    return !video.paused && !video.ended && !video.muted && video.volume > 0;
  }

  // True while the button has a reason to stay without the pointer. Not in full screen,
  // where it would sit in the picture. A child frame's word for it lasts KEEP_FOR_MS (it
  // repeats it a few times a second while its video plays).
  function stays() {
    if (document.fullscreenElement) return false;
    if (current) return current === pinned && current.isConnected && audible(current);
    return !!remote?.keep && Date.now() - remote.at < KEEP_FOR_MS;
  }

  // A video taken out of the page, or a frame that went away, sends no event: what stays is
  // looked at again when nothing has confirmed it for a while.
  const KEEP_FOR_MS = 1500;
  let keepTimer = 0;
  function confirmStay() {
    clearTimeout(keepTimer);
    keepTimer = setTimeout(() => {
      if (!(current || remote)) return;
      if (stays()) confirmStay();
      else hideSoon();
    }, KEEP_FOR_MS + 100);
  }

  chrome.storage.local.get("capture").then(({ capture = true }) => (enabled = capture));
  chrome.storage.onChanged.addListener((changes) => {
    if (changes.capture) {
      enabled = changes.capture.newValue !== false;
      if (!enabled) hide();
    }
  });

  function build() {
    host = document.createElement("rdm-video");
    host.style.cssText = "all: initial; position: fixed; z-index: 2147483647; top: 0; left: 0; display: none;";
    const root = host.attachShadow({ mode: "closed" });
    root.innerHTML = `
      <style>
        .bar { display: flex; gap: 2px; }
        button {
          all: initial; box-sizing: border-box; display: flex; align-items: center; gap: 2px;
          height: 22px; padding: 0 6px; border-radius: 4px; cursor: pointer; color: #fff;
          background: rgba(20, 24, 32, 0.85); border: 1px solid rgba(255, 255, 255, 0.25);
          font: 400 11px "Segoe UI", system-ui, -apple-system, sans-serif;
          box-shadow: 0 1px 4px rgba(0, 0, 0, 0.4);
        }
        button:hover { background: #2f78d6; border-color: #2f78d6; }
        button:focus-visible { outline: 2px solid #4c9be8; outline-offset: 1px; }
        .download.sent { background: #2e7d32; border-color: #2e7d32; }
        .download.failed { background: #b3261e; border-color: #b3261e; }
        svg { width: 12px; height: 12px; fill: currentColor; }
        .close { padding: 0 5px; opacity: 0.8; }
        .close:hover { background: #b3261e; border-color: #b3261e; opacity: 1; }
      </style>
      <div class="bar">
        <button type="button" class="download" title="${IDLE_TITLE}" aria-label="${IDLE_TITLE}">
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 2.5v11l9-5.5z"/></svg>
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M7 1h2v7.6l2.6-2.6L13 7.4 8 12.4 3 7.4 4.4 6 7 8.6zM2 13h12v2H2z"/></svg>
        </button>
        <button type="button" class="close" title="Hide for this video" aria-label="Hide for this video">✕</button>
      </div>`;
    button = root.querySelector(".download");
    button.addEventListener("click", send);
    root.querySelector(".close").addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      dismiss();
      hide();
    });
    host.addEventListener("pointerenter", () => clearTimeout(hideTimer));
    host.addEventListener("pointerleave", hideSoon);
    (document.body || document.documentElement).append(host);
  }

  function send(event) {
    event.preventDefault();
    event.stopPropagation();
    const page = remote ? remote.page : location.href;
    const file = remote ? remote.playing : current && fileOf(current);
    const show = (state, title) => {
      button.classList.remove("sent", "failed");
      if (state) button.classList.add(state);
      button.title = title;
    };
    // After the extension was loaded anew (an update of RDM), this script of the copy
    // before reaches nobody; the new copy's button comes with the page's next load.
    if (!chrome.runtime?.id) return show("failed", "RDM was updated: reload this page to use this button");
    show(null, "Opening RDM…");
    chrome.runtime.sendMessage({ video: page, playing: file || null }).then(
      (reply) => {
        show(reply?.ok ? "sent" : "failed", reply?.ok ? "Sent to RDM" : "RDM did not answer");
        setTimeout(() => show(null, IDLE_TITLE), 2500);
      },
      () => show(null, IDLE_TITLE),
    );
  }

  // The ✕: remembered by the frame that holds the video.
  function dismiss() {
    if (current) dismissed.set(current, playing(current));
    else if (remote) post(remote.source, { dismiss: true });
  }

  function large(r) {
    return r.width >= MIN_WIDTH && r.height >= MIN_HEIGHT;
  }

  function post(target, message) {
    try {
      target.postMessage({ rdmVideo: message }, "*");
    } catch {
      // The frame is gone.
    }
  }

  // The <iframe> element in this page whose window is `source`.
  function frameOf(source) {
    return [...document.querySelectorAll("iframe, frame")].find((f) => f.contentWindow === source) || null;
  }

  // The video's box in this frame's viewport, for this frame's video or a child's.
  function videoBox() {
    if (current) return current.isConnected ? current.getBoundingClientRect() : null;
    const frame = remote && frameOf(remote.source);
    if (!frame) return null;
    const f = frame.getBoundingClientRect();
    const style = getComputedStyle(frame);
    const left = f.left + frame.clientLeft + parseFloat(style.paddingLeft);
    const top = f.top + frame.clientTop + parseFloat(style.paddingTop);
    const r = remote.rect;
    return { left: left + r.left, top: top + r.top, right: left + r.right, bottom: top + r.bottom, width: r.width, height: r.height };
  }

  function place() {
    if (!host || !(current || remote)) return;
    const r = videoBox();
    if (!r || !large(r) || r.bottom < 0 || r.top > innerHeight) {
      hide();
      return;
    }
    host.style.display = "block";
    const height = host.offsetHeight || 22;
    const room = r.top >= height + GAP;
    // In full screen only this frame is on screen.
    if (!room && window !== window.top && !document.fullscreenElement) {
      // The page around this frame draws it, above the frame.
      host.style.display = "none";
      forwarded = true;
      const box = { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
      post(window.parent, {
        show: true,
        rect: box,
        page: remote ? remote.page : location.href,
        playing: remote ? remote.playing : fileOf(current),
        keep: stays(),
      });
      return;
    }
    if (forwarded) post(window.parent, { hide: true });
    forwarded = false;
    const width = host.offsetWidth || 60;
    host.style.left = `${Math.max(4, Math.min(r.right, innerWidth) - width)}px`;
    host.style.top = `${room ? r.top - height - GAP : Math.max(4, r.top + 6)}px`;
  }

  function show(video) {
    if (!enabled || !large(video.getBoundingClientRect()) || dismissed.get(video) === playing(video)) return;
    if (!host) build();
    clearTimeout(hideTimer);
    current = video;
    remote = null;
    place();
  }

  function hide() {
    clearTimeout(hideTimer);
    if (forwarded) post(window.parent, { hide: true });
    forwarded = false;
    current = null;
    remote = null;
    if (host) host.style.display = "none";
  }

  function hideSoon() {
    clearTimeout(hideTimer);
    if (stays()) return;
    // (On its way out: the look taken later at what stays would only start the wait again.)
    clearTimeout(keepTimer);
    if (forwarded) {
      // The button is in an ancestor, where the pointer may be heading: it decides.
      post(window.parent, { leave: true });
      forwarded = false;
      current = null;
      remote = null;
      return;
    }
    hideTimer = setTimeout(hide, LINGER_MS);
  }

  // From a child frame: draw (or drop) the button for its video. From the parent: the
  // button it drew for this frame was closed.
  addEventListener("message", (e) => {
    const m = e.data?.rdmVideo;
    if (!m || typeof m !== "object") return;
    if (m.dismiss && e.source === window.parent) {
      dismiss();
      current = null;
      remote = null;
      forwarded = false;
      return;
    }
    if (!e.source || e.source === window || !frameOf(e.source)) return;
    const box = m.rect && ["left", "top", "right", "bottom", "width", "height"].every((k) => Number.isFinite(m.rect[k]));
    if (m.show && enabled && box && typeof m.page === "string") {
      const keep = m.keep === true;
      // A frame's playing video does not take the button from a video of this page.
      if (keep && current) return;
      if (!host) build();
      clearTimeout(hideTimer);
      current = null;
      remote = { source: e.source, rect: m.rect, page: m.page, playing: typeof m.playing === "string" ? m.playing : null, keep, at: Date.now() };
      if (keep) confirmStay();
      place();
    } else if (remote?.source === e.source) {
      if (m.hide) hide();
      else if (m.leave) {
        remote.keep = false;
        hideSoon();
      }
    }
  });

  // Players often cover the video with their own controls, so the pointer is matched by
  // position rather than by the element under it. Checked once per frame at most.
  let pointer = null;
  // The video under the pointer, as last looked.
  let hovered = null;
  function track() {
    const { x, y } = pointer;
    pointer = null;
    const video = [...document.querySelectorAll("video")].find((v) => covers(v, x, y)) || shadowVideoAt(x, y, document);
    hovered = video || null;
    if (video) show(video);
    else if (current || remote) hideSoon();
  }

  function covers(v, x, y) {
    const r = v.getBoundingClientRect();
    return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
  }

  // A video inside a player's open shadow root (web-component players), found through the
  // elements at the point, level by level.
  function shadowVideoAt(x, y, root, depth = 0) {
    for (const el of root.elementsFromPoint(x, y)) {
      if (el instanceof HTMLVideoElement) return el;
      if (el.shadowRoot && depth < 4) {
        const inner = shadowVideoAt(x, y, el.shadowRoot, depth + 1);
        if (inner) return inner;
      }
    }
    return null;
  }

  document.addEventListener(
    "pointermove",
    (e) => {
      if (!enabled || (host && e.composedPath().includes(host))) return;
      if (!pointer) requestAnimationFrame(track);
      pointer = { x: e.clientX, y: e.clientY };
    },
    { passive: true, capture: true },
  );
  // Into a frame or out of the window: this page sees no more moves to act on.
  document.addEventListener(
    "pointerout",
    (e) => {
      const into = e.relatedTarget;
      const away = !into || into instanceof HTMLIFrameElement || into instanceof HTMLFrameElement;
      if (away) hovered = null;
      if ((current || remote) && away) hideSoon();
    },
    { passive: true, capture: true },
  );
  addEventListener("scroll", place, { passive: true, capture: true });
  addEventListener("resize", place, { passive: true });
  document.addEventListener("fullscreenchange", () => {
    // What stayed without the pointer goes when the screen fills; the pointer brings it back.
    if (document.fullscreenElement && (current ? current === pinned : remote?.keep)) hide();
    else place();
  });

  // A video that plays with sound gets the button without the pointer, and keeps it until
  // it stops or is muted. While it plays the button follows the page's layout (timeupdate
  // comes a few times a second).
  function watch(e) {
    const v = e.target;
    if (!(v instanceof HTMLVideoElement)) return;
    if (enabled && audible(v) && !document.fullscreenElement) {
      pinned = v;
      confirmStay();
      if (current === v) {
        clearTimeout(hideTimer);
        place();
      } else if (!current && !remote) {
        show(v);
      }
    } else if (pinned === v) {
      pinned = null;
      clearTimeout(keepTimer);
      // Under the pointer it stays as long as the pointer does. (An ancestor that draws
      // the button is told by hideSoon that it need not stay.)
      if (current === v && hovered !== v) hideSoon();
    }
  }
  for (const type of ["playing", "timeupdate", "volumechange", "pause", "ended", "emptied"]) {
    document.addEventListener(type, watch, { capture: true, passive: true });
  }

  // Listing in advance: once a video has played a moment with sound, RDM looks up
  // its qualities in the background, so the list is ready when the button is pressed.
  // YouTube only (other sites may take repeated lookups for a bot); muted videos are left
  // out, which also leaves out YouTube's hover previews on its home and search pages.
  const AHEAD_AFTER_S = 1.5;
  const listedAhead = new Set();
  document.addEventListener(
    "timeupdate",
    (e) => {
      const v = e.target;
      if (!enabled || !(v instanceof HTMLVideoElement) || v.muted || v.volume === 0) return;
      if (!/(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname)) return;
      const key = playing(v);
      if (listedAhead.has(key) || !large(v.getBoundingClientRect())) return;
      let seconds = 0;
      for (let i = 0; i < v.played.length; i++) seconds += v.played.end(i) - v.played.start(i);
      if (seconds < AHEAD_AFTER_S) return;
      listedAhead.add(key);
      chrome.runtime.sendMessage({ ahead: location.href }).catch(() => {});
    },
    { capture: true, passive: true },
  );

  // Playlists sniff.js read in this page: catch.js lists them with what the tab played.
  addEventListener("message", (e) => {
    const found = e.source === window ? e.data?.rdmPlaylist : null;
    if (found && typeof found.url === "string" && /^(hls|dash)$/.test(found.kind)) {
      const text = typeof found.text === "string" ? found.text : null;
      chrome.runtime.sendMessage({ playlist: { url: found.url, kind: found.kind, text } }).catch(() => {});
    }
    // The format list YouTube's player was given (sniff.js).
    const formats = e.source === window && /(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname) ? e.data?.rdmFormats : null;
    if (formats && typeof formats.id === "string" && Array.isArray(formats.formats)) {
      chrome.runtime.sendMessage({ formats }).catch(() => {});
    }
  });
})();
