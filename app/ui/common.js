// Shared by every window.
const { invoke } = window.__TAURI__.core;
const thisWindow = window.__TAURI__.window.getCurrentWindow();

const el = (id) => document.getElementById(id);
const param = (name) => new URLSearchParams(location.search).get(name);

function human(bytes) {
  const units = ["Bytes", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return u === 0 ? `${v} Bytes` : `${v.toFixed(u > 1 ? 2 : 1)} ${units[u]}`;
}

function rate(bytesPerSecond) {
  return bytesPerSecond > 0 ? `${human(bytesPerSecond)}/sec` : "";
}

function clock(seconds) {
  if (!isFinite(seconds) || seconds < 0) return "";
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  if (h > 0) return `${h} hr ${m} min`;
  if (m > 0) return `${m} min ${sec} sec`;
  return `${s} sec`;
}

// A download's size, with ~ while it is a guess (a playlist without byte ranges).
function sizeText(d) {
  return d.size != null ? `${d.size_estimated ? "~" : ""}${human(d.size)}` : "";
}

function timeLeft(d) {
  return d.status === "downloading" && d.size && d.speed > 0 ? clock((d.size - d.downloaded) / d.speed) : "";
}

function percent(d) {
  return d.size ? Math.min(100, (d.downloaded * 100) / d.size) : 0;
}

const STATUS_TEXT = {
  waiting: "Queued",
  downloading: "Receiving data...",
  paused: "Stopped",
  complete: "Complete",
  failed: "Error",
};

function statusText(d) {
  if (d.status === "downloading" && d.size) return `${percent(d).toFixed(2)}%`;
  // The check by file type found a problem (the main list shows why as a tooltip).
  if (d.status === "complete" && d.file_check?.ok === false) return "Complete, may be damaged";
  return STATUS_TEXT[d.status] || d.status;
}

// The saved file's name once complete (it can differ: "name (1).zip"), else the planned one.
function fileNameOf(d) {
  if (d.path) return splitPath(d.path)[1];
  if (d.file_name) return d.file_name;
  try {
    // A video page's address says nothing about the file; its title comes from yt-dlp.
    if (d.video) return `Video from ${new URL(d.url).hostname.replace(/^www\./, "")}`;
    return decodeURIComponent(new URL(d.url).pathname.split("/").filter(Boolean).pop() || d.url);
  } catch {
    return d.url;
  }
}

function separatorOf(path) {
  return path.includes("\\") ? "\\" : "/";
}

function joinPath(dir, name) {
  if (!dir) return name;
  const sep = separatorOf(dir);
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
}

function splitPath(path) {
  const i = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return i < 0 ? ["", path] : [path.slice(0, i), path.slice(i + 1)];
}

// Draws each segment's written part over the whole file, as IDM's segment bar.
function drawSegments(canvas, d) {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  canvas.width = width * devicePixelRatio;
  canvas.height = height * devicePixelRatio;
  const g = canvas.getContext("2d");
  g.scale(devicePixelRatio, devicePixelRatio);
  const styles = getComputedStyle(document.documentElement);
  g.fillStyle = styles.getPropertyValue("--bar-back");
  g.fillRect(0, 0, width, height);
  if (!d.size) return;
  g.fillStyle = styles.getPropertyValue("--accent");
  const whole = d.status === "complete" ? [{ start: 0, pos: d.size, end: d.size }] : d.segments;
  for (const s of whole) {
    const x = (s.start / d.size) * width;
    g.fillRect(x, 0, Math.max(((s.pos - s.start) / d.size) * width, s.pos > s.start ? 1 : 0), height);
  }
}

// The Checked and File check rows (complete and Properties windows): the server's hash,
// checked before the file got its name, and the check by file type after it.
function showChecked(d) {
  el("checkedrow").hidden = !d.checked;
  el("checked").textContent = d.checked ? `${d.checked}: matches` : "";
  el("checked").title = el("checked").textContent;
}

function showFileCheck(d) {
  el("filerow").hidden = !d.checking && !d.file_check;
  el("filecheck").textContent = d.checking ? "Checking the file..." : d.file_check?.text ?? "";
  el("filecheck").classList.toggle("error", d.file_check?.ok === false);
}

// The Checksum row (complete and Properties windows): a hash from the site (its length
// tells the kind), or none for the file's SHA-256. The window keeps `width` and its height
// follows the answer.
function wireChecksum(id, width) {
  const answer = async (text, bad) => {
    el("sumrow").hidden = false;
    el("sumresult").textContent = text;
    el("sumresult").classList.toggle("error", bad);
    await fitWindow(width);
  };
  el("check").addEventListener("click", async () => {
    el("check").disabled = true;
    await answer("Computing...", false);
    try {
      const c = await invoke("check_hash", { id, expected: el("sum").value });
      if (c.matches === null) await answer(`${c.kind}: ${c.hash}`, false);
      else if (c.matches) await answer(`Matches (${c.kind})`, false);
      else await answer(`Does not match. The file's ${c.kind} is ${c.hash}`, true);
    } catch (e) {
      await answer(String(e), true);
    } finally {
      el("check").disabled = false;
    }
  });
  el("sum").addEventListener("keydown", (e) => {
    if (e.key === "Enter") el("check").click();
  });
}

// Makes a fixed-width window as tall as its content (body.fit). The size is given in the
// page's own pixels: the page can be drawn at another scale than the window is sized in
// (seen on a 4K screen: 200 % in a window at 175 %), and a size in the window's scale then
// cuts the page off. The content is measured at the width the window is about to have,
// and to the fraction of a pixel (offsetHeight is rounded, at times down).
async function fitWindow(width) {
  document.body.style.width = `${width}px`;
  const pixels = (length) => Math.ceil(length * devicePixelRatio);
  const height = document.body.getBoundingClientRect().height;
  await thisWindow.setSize(new window.__TAURI__.window.PhysicalSize(pixels(width), pixels(height)));
}

// Shows a window that opened hidden (windows.rs) once `prepare` has filled and fitted it.
async function showWhenReady(prepare) {
  try {
    await prepare();
    await thisWindow.center();
  } finally {
    await thisWindow.show();
    await thisWindow.setFocus();
  }
}

// Escape closes a dialog, as on Windows.
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape" || !document.body.classList.contains("dialog")) return;
  const cancel = el("cancel");
  if (cancel) cancel.click();
  else thisWindow.close();
});
