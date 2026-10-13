// Download progress for one download, refreshed twice a second.

const id = Number(param("id"));
let current = null;
// "Show details" carries over to the next download's window.
const DETAILS_KEY = "progress.details";
let detailed = false;
try {
  detailed = localStorage.getItem(DETAILS_KEY) === "1";
} catch {}

async function refresh() {
  let d;
  try {
    d = await invoke("get_download", { id });
  } catch {
    // Removed from the list.
    await thisWindow.close();
    return;
  }
  current = d;
  const name = fileNameOf(d);
  el("name").textContent = name;
  el("url").textContent = d.url;
  el("url").title = d.url;
  // Before the server (or, for a video page, yt-dlp) has answered, nothing is known yet.
  const starting = d.status === "downloading" && d.size == null && d.downloaded === 0 && d.segments.length === 0;
  el("status").textContent =
    d.status === "failed"
      ? `Error: ${d.error || "failed"}`
      : starting
        ? d.video
          ? "Looking up the video..."
          : "Connecting..."
        : STATUS_TEXT[d.status];
  el("size").textContent = d.size != null ? sizeText(d) : starting ? "" : "Unknown";
  el("done").textContent = d.size ? `${human(d.downloaded)} (${percent(d).toFixed(2)} %)` : human(d.downloaded);
  el("rate").textContent = d.status === "downloading" ? rate(d.speed) : "";
  el("left").textContent = timeLeft(d);
  // A playlist resumes piece by piece; a file when the server takes ranges (segments).
  const resumable = d.pieces != null || d.segments.length > 0;
  el("resume").textContent = resumable ? "Yes" : d.status === "downloading" && !starting ? "No" : "";
  el("fill").style.width = `${percent(d)}%`;
  el("pause").textContent = d.status === "downloading" ? "Pause" : "Resume";
  el("pause").disabled = d.status === "complete";
  await thisWindow.setTitle(d.size ? `${Math.floor(percent(d))}% ${name}` : name);
  if (detailed) {
    drawSegments(el("segments"), d);
    el("connections").textContent = String(d.active_connections);
    // A playlist comes as many small pieces, written in order, not as segments.
    el("segname").textContent = d.pieces ? "Pieces" : "Segments";
    el("segcount").textContent = d.pieces ? `${d.pieces[0]} of ${d.pieces[1]}` : String(d.segments.length);
  }
}

el("pause").addEventListener("click", async () => {
  if (!current) return;
  el("pause").disabled = true;
  try {
    if (current.status === "downloading") await invoke("pause", { id });
    else await invoke("resume", { id });
  } finally {
    el("pause").disabled = false;
    refresh();
  }
});

// As in IDM, Cancel stops the download and closes the window; it stays in the list.
el("cancel").addEventListener("click", async () => {
  if (current?.status === "downloading") await invoke("pause", { id });
  await thisWindow.close();
});

// The window is as tall as its content, with or without the details.
async function showDetails() {
  el("details").hidden = !detailed;
  el("more").innerHTML = detailed ? "&lt;&lt; Hide details" : "Show details &gt;&gt;";
  await fitWindow(500);
}

el("more").addEventListener("click", async () => {
  detailed = !detailed;
  try {
    localStorage.setItem(DETAILS_KEY, detailed ? "1" : "0");
  } catch {}
  await showDetails();
  refresh();
});

(async () => {
  await showWhenReady(async () => {
    // Filled first: an empty name line has no height.
    await refresh();
    await showDetails();
    if (detailed) await refresh();
  });
  setInterval(refresh, 500);
})();
