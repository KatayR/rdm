// Download complete: open the file or its folder.

const id = Number(param("id"));

async function load() {
  const d = await invoke("get_download", { id });
  await thisWindow.setTitle(`Download complete: ${fileNameOf(d)}`);
  el("size").textContent = `${human(d.size ?? d.downloaded)} (${(d.size ?? d.downloaded).toLocaleString()} Bytes)`;
  el("url").textContent = d.url;
  el("url").title = d.url;
  el("path").textContent = d.path;
  el("path").title = d.path;
  showChecked(d);
  followFileCheck(d);
  el("open").focus();
}

// The check by file type runs once the download is complete; followed until it is done.
function followFileCheck(d) {
  showFileCheck(d);
  if (!d.checking) return;
  setTimeout(async () => {
    try {
      followFileCheck(await invoke("get_download", { id }));
      await fitWindow(520);
    } catch {
      // Removed from the list meanwhile.
    }
  }, 500);
}

// Every button closes the window; the checkbox is saved first.
async function finish(action) {
  try {
    if (action) await invoke(action, { id });
    if (el("dontshow").checked) {
      const settings = await invoke("settings");
      await invoke("update_settings", { settings: { ...settings, show_complete: false } });
    }
    await thisWindow.close();
  } catch (e) {
    el("error").textContent = String(e);
  }
}

wireChecksum(id, 520);

el("open").addEventListener("click", () => finish("open_file"));
el("folder").addEventListener("click", () => finish("open_folder"));
el("cancel").addEventListener("click", () => finish(null));

showWhenReady(async () => {
  await load().catch((e) => (el("error").textContent = String(e)));
  await fitWindow(520);
});
