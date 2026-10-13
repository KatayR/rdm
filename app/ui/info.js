// Download File Info: confirm a new download, choose its folder and name.

const token = Number(param("token"));
let pending = null;
let categories = [];
let pathEdited = false;
let answered = false;

// What is known at once; the window is shown when this is filled.
async function load() {
  pending = await invoke("pending", { token });
  el("url").value = pending.url;
  categories = await invoke("categories");
  for (const c of categories) el("category").add(new Option(c.name, c.name));
  for (const q of await invoke("queues")) el("queue").add(new Option(q.name, q.id));
  await placeFile(pending.file_name || fileNameOf({ url: pending.url }));
  await showDuplicate();
  el("start").focus();
}

// The same address is in the list already: say so, with what can be done instead.
async function showDuplicate() {
  const d = await invoke("duplicate_of", { token });
  el("dupe").hidden = !d;
  if (!d) return;
  const status = d.status === "failed" ? "failed" : d.status === "complete" ? "finished" : STATUS_TEXT[d.status].toLowerCase();
  el("dupetext").textContent = `Already in the list: ${fileNameOf(d)} (${d.status === "downloading" ? statusText(d) : status})`;
  el("dupecontinue").hidden = d.status === "complete";
  el("dupeshow").onclick = () => invoke("show_download", { id: d.id }).catch((e) => (el("error").textContent = String(e)));
  el("dupecontinue").onclick = async () => {
    try {
      await invoke("continue_duplicate", { token });
      answered = true;
      await thisWindow.close();
    } catch (e) {
      el("error").textContent = String(e);
    }
  };
}

// The same file (name and size) is being downloaded already and did not finish: this link
// may be a fresh one for it (the old one expired), as IDM's "refresh download address".
async function showSameFile() {
  const d = await invoke("same_file", { token });
  el("same").hidden = !d;
  if (!d) return;
  const done = d.size ? `${percent(d).toFixed(1)}% done` : `${human(d.downloaded)} done`;
  el("sametext").textContent = `Looks like the unfinished ${fileNameOf(d)} (${done})`;
  el("sameuse").onclick = async () => {
    try {
      await invoke("refresh_address", { token, id: d.id });
      answered = true;
      await thisWindow.close();
    } catch (e) {
      el("error").textContent = String(e);
    }
  };
}

// What the server says, which can take a while.
async function check() {
  try {
    // The download started when this window opened (as in IDM); what its first request
    // learned is read from it. Without one, the server is asked here.
    const early = await invoke("early_download", { token });
    const info = early != null ? await serverAnswer(early) : await invoke("check_url", { url: pending.url, headers: pending.headers });
    // The download's own folder and name (it may have taken "name (1)" beside another).
    await placeFile(info.file_name, info.dir);
    if (early != null) await showSameFile();
    el("size").textContent =
      info.size != null ? `${human(info.size)} (${info.size.toLocaleString()} Bytes)` : "Size unknown";
    el("ranges").textContent = `· Resume capability: ${info.ranges ? "Yes" : "No"}`;
  } catch (e) {
    el("size").textContent = "";
    el("error").textContent = `The server check failed: ${e}`;
  }
  await fitWindow(580);
}

// Waits until the early download's server has answered: name, size, ranges.
async function serverAnswer(id) {
  for (;;) {
    const d = await invoke("get_download", { id });
    if (d.status === "failed") throw d.error || "failed";
    if (d.resumable != null || d.status === "complete") {
      return { file_name: d.file_name, dir: d.dir, size: d.size, ranges: d.resumable === true };
    }
    await new Promise((r) => setTimeout(r, 100));
  }
}

// Picks the category by file name and puts the file in its folder, unless the person
// already edited the path.
async function placeFile(name, dir = null) {
  if (pathEdited) return;
  const category = await invoke("category_for", { fileName: name });
  if (category) el("category").value = category.name;
  el("path").value = joinPath(dir || pending.dir || category?.dir || "", name);
}

el("category").addEventListener("change", () => {
  const category = categories.find((c) => c.name === el("category").value);
  const [, name] = splitPath(el("path").value);
  el("path").value = joinPath(category.dir, name);
});

el("path").addEventListener("input", () => (pathEdited = true));

el("browse").addEventListener("click", async () => {
  const [dir, name] = splitPath(el("path").value);
  const folder = await invoke("browse_folder", { start: dir || null });
  if (folder) {
    el("path").value = joinPath(folder, name);
    pathEdited = true;
  }
});

async function answer(start) {
  el("error").textContent = "";
  try {
    await invoke("confirm", {
      token,
      start,
      path: el("path").value.trim(),
      description: el("description").value,
      queue: start ? null : Number(el("queue").value),
    });
    answered = true;
    await thisWindow.close();
  } catch (e) {
    el("error").textContent = String(e);
  }
}

el("start").addEventListener("click", () => answer(true));
el("later").addEventListener("click", () => answer(false));
el("cancel").addEventListener("click", () => thisWindow.close());
document.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && document.activeElement?.tagName !== "BUTTON") answer(true);
});

// Closing the window in any way drops a download that was not answered.
thisWindow.onCloseRequested(async () => {
  if (!answered) await invoke("cancel_pending", { token });
});

showWhenReady(async () => {
  await load().catch((e) => (el("error").textContent = String(e)));
  await fitWindow(580);
}).then(check);
