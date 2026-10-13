// Properties of one download (right-click in the list): what is known about it; only the
// description can be changed.

const id = Number(param("id"));
const WIDTH = 560;

// The parts that change while it downloads; the description field is left alone.
function show(d) {
  const name = fileNameOf(d);
  el("name").textContent = name;
  el("url").textContent = d.url;
  const done = d.status === "complete" && d.path;
  el("placename").textContent = done ? "Saved as" : "Folder";
  el("place").textContent = done ? d.path : d.dir || "Not decided yet";
  el("category").textContent = d.category || "";
  el("size").textContent =
    d.size != null ? `${sizeText(d)} (${d.size.toLocaleString()} Bytes)${d.status === "complete" ? "" : `, ${human(d.downloaded)} downloaded`}` : d.downloaded ? `${human(d.downloaded)} downloaded` : "Unknown";
  el("status").textContent = d.status === "failed" ? `Error: ${d.error || "failed"}` : statusText(d);
  el("status").classList.toggle("error", d.status === "failed");
  el("added").textContent = new Date(d.added * 1000).toLocaleString();
  el("lasttry").textContent = d.last_try ? new Date(d.last_try * 1000).toLocaleString() : "";
  showChecked(d);
  showFileCheck(d);
  // A hash can be checked once the file is there.
  el("sumfield").hidden = !done;
}

async function refresh() {
  try {
    show(await invoke("get_download", { id }));
    await fitWindow(WIDTH);
  } catch {
    // Removed from the list.
    await thisWindow.close();
  }
}

el("ok").addEventListener("click", async () => {
  try {
    await invoke("set_description", { id, description: el("description").value });
    await thisWindow.close();
  } catch (e) {
    el("error").textContent = String(e);
  }
});
el("cancel").addEventListener("click", () => thisWindow.close());
el("folder").addEventListener("click", () => invoke("open_folder", { id }).catch((e) => (el("error").textContent = String(e))));
el("description").addEventListener("keydown", (e) => {
  if (e.key === "Enter") el("ok").click();
});
wireChecksum(id, WIDTH);

showWhenReady(async () => {
  const d = await invoke("get_download", { id });
  await thisWindow.setTitle(`Properties: ${fileNameOf(d)}`);
  el("description").value = d.description || "";
  show(d);
  await fitWindow(WIDTH);
  el("ok").focus();
}).then(() => setInterval(refresh, 1000));
