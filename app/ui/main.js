// Main window: the download list with IDM's categories, columns and toolbar.

let downloads = [];
let categories = [];
let filter = { kind: "all" };
const selected = new Set();

const FILTERS = (d) => ({
  all: true,
  category: d.category === filter.name,
  unfinished: d.status !== "complete",
  finished: d.status === "complete",
});

function shown() {
  return downloads.filter((d) => FILTERS(d)[filter.kind]);
}

function renderTree() {
  const tree = el("tree");
  tree.textContent = "";
  const item = (text, value, cls) => {
    const div = document.createElement("div");
    div.textContent = text;
    div.className = `selectable ${cls || ""}`;
    if (filter.kind === value.kind && filter.name === value.name) div.classList.add("selected");
    div.addEventListener("click", () => {
      filter = value;
      selected.clear();
      renderTree();
      renderRows();
    });
    tree.append(div);
  };
  item("All Downloads", { kind: "all" });
  for (const c of categories) item(c.name, { kind: "category", name: c.name }, "child");
  item("Unfinished", { kind: "unfinished" });
  item("Finished", { kind: "finished" });
}

function cell(text, cls) {
  const td = document.createElement("td");
  td.textContent = text;
  td.title = text;
  if (cls) td.className = cls;
  return td;
}

function statusCell(d) {
  const td = cell(d.status === "failed" ? `Error: ${d.error || ""}` : statusText(d));
  if (d.status === "complete" && d.file_check?.ok === false) td.title = d.file_check.text;
  return td;
}

// The list's columns: header, width in px, and the value it sorts by (null sorts last).
const COLUMNS = [
  { key: "name", title: "File Name", width: 233, value: (d) => fileNameOf(d) },
  { key: "size", title: "Size", width: 78, value: (d) => d.size ?? null },
  { key: "status", title: "Status", width: 85, value: (d) => (d.status === "failed" ? `Error: ${d.error || ""}` : statusText(d)) },
  { key: "left", title: "Time left", width: 85, value: (d) => (timeLeft(d) ? (d.size - d.downloaded) / d.speed : null) },
  { key: "rate", title: "Transfer rate", width: 93, value: (d) => (d.status === "downloading" && d.speed > 0 ? d.speed : null) },
  { key: "last", title: "Last Try Date", width: 101, value: (d) => d.last_try ?? null },
  { key: "description", title: "Description", width: 101, value: (d) => d.description || null },
];
const MIN_WIDTH = 40;

// Widths and sort order are the person's choice and survive restarts (per viewer, in the
// webview's storage; the defaults when that is unavailable).
function stored(name, fallback) {
  try {
    return JSON.parse(localStorage.getItem(name)) ?? fallback;
  } catch {
    return fallback;
  }
}
function store(name, value) {
  try {
    localStorage.setItem(name, JSON.stringify(value));
  } catch {}
}
const widths = { ...Object.fromEntries(COLUMNS.map((c) => [c.key, c.width])), ...stored("list.widths", {}) };
let sort = stored("list.sort", null); // { key, descending }

function renderHeads() {
  const cols = el("cols");
  const heads = el("heads");
  cols.textContent = "";
  heads.textContent = "";
  for (const c of COLUMNS) {
    const col = document.createElement("col");
    col.style.width = `${widths[c.key]}px`;
    cols.append(col);
    const th = document.createElement("th");
    th.textContent = c.title;
    th.title = c.title;
    if (sort?.key === c.key) {
      const arrow = document.createElement("span");
      arrow.className = "arrow";
      arrow.textContent = sort.descending ? "▼" : "▲";
      th.append(arrow);
    }
    const grip = document.createElement("span");
    grip.className = "grip";
    grip.addEventListener("mousedown", (e) => startResize(e, c.key, col));
    th.append(grip);
    // First click sorts ascending, the next one descending.
    th.addEventListener("click", (e) => {
      if (e.target === grip || resizing) return;
      sort = sort?.key === c.key ? { key: c.key, descending: !sort.descending } : { key: c.key, descending: false };
      store("list.sort", sort);
      renderHeads();
      renderRows();
    });
    heads.append(th);
  }
  el("downloads").style.width = `${COLUMNS.reduce((sum, c) => sum + widths[c.key], 0)}px`;
}

let resizing = false;
function startResize(e, key, col) {
  e.preventDefault();
  resizing = true;
  const startX = e.clientX;
  const startWidth = widths[key];
  const move = (m) => {
    widths[key] = Math.max(MIN_WIDTH, Math.round(startWidth + m.clientX - startX));
    col.style.width = `${widths[key]}px`;
    el("downloads").style.width = `${COLUMNS.reduce((sum, c) => sum + widths[c.key], 0)}px`;
  };
  const up = () => {
    document.removeEventListener("mousemove", move);
    document.removeEventListener("mouseup", up);
    store("list.widths", widths);
    // The click that ends a drag is not a sort.
    setTimeout(() => (resizing = false));
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", up);
}

// Unknown values last in both directions; text by its natural order ("File 2" before
// "File 10"); ties keep the list's own order.
function sorted(list) {
  const column = COLUMNS.find((c) => c.key === sort?.key);
  if (!column) return list;
  const text = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
  const keyed = list.map((d, i) => ({ d, i, v: column.value(d) }));
  keyed.sort((a, b) => {
    if (a.v === null || b.v === null) return a.v === b.v ? a.i - b.i : a.v === null ? 1 : -1;
    const order = typeof a.v === "string" ? text.compare(a.v, b.v) : a.v - b.v;
    return (sort.descending ? -order : order) || a.i - b.i;
  });
  return keyed.map((k) => k.d);
}

function renderRows() {
  const body = el("rows");
  body.textContent = "";
  for (const d of sorted(shown())) {
    const tr = document.createElement("tr");
    if (selected.has(d.id)) tr.className = "selected";
    tr.append(
      cell(fileNameOf(d)),
      cell(sizeText(d), "num"),
      statusCell(d),
      cell(timeLeft(d)),
      cell(d.status === "downloading" ? rate(d.speed) : ""),
      cell(d.last_try ? new Date(d.last_try * 1000).toLocaleString() : ""),
      cell(d.description || ""),
    );
    tr.addEventListener("click", (e) => {
      // Ctrl adds to the selection; on a Mac, where Ctrl+click is the right click, Cmd does.
      if (!(isMac ? e.metaKey : e.ctrlKey)) selected.clear();
      if (selected.has(d.id)) selected.delete(d.id);
      else selected.add(d.id);
      renderRows();
    });
    // As on Windows: a right click on a row outside the selection selects that row alone.
    tr.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      if (!selected.has(d.id)) {
        selected.clear();
        selected.add(d.id);
        renderRows();
      }
      showRowMenu(d.id, e.clientX, e.clientY);
    });
    tr.addEventListener("dblclick", () =>
      d.status === "complete" ? act(() => invoke("open_file", { id: d.id })) : invoke("show_progress", { id: d.id }),
    );
    body.append(tr);
  }
  const running = downloads.filter((d) => d.status === "downloading");
  const speed = running.reduce((sum, d) => sum + d.speed, 0);
  el("summary").textContent = `${downloads.length} downloads · ${running.length} running${speed ? " · " + rate(speed) : ""}`;
}

// The right-click menu, kept inside the window; what does not apply to the row is greyed.
let menuFor = null;
function showRowMenu(id, x, y) {
  menuFor = id;
  const d = downloads.find((x) => x.id === id);
  const status = d?.status;
  const can = {
    menuopen: status === "complete",
    menuresume: status && status !== "complete" && status !== "downloading",
    menustop: status === "downloading",
  };
  for (const [item, on] of Object.entries(can)) el(item).classList.toggle("disabled", !on);
  const menu = el("rowmenu");
  menu.hidden = false;
  menu.style.left = `${Math.min(x, innerWidth - menu.offsetWidth - 2)}px`;
  menu.style.top = `${Math.min(y, innerHeight - menu.offsetHeight - 2)}px`;
}
function hideRowMenu() {
  el("rowmenu").hidden = true;
}
// Each item: what it does for the row the menu is for.
const MENU = {
  menuopen: (id) => act(() => invoke("open_file", { id })),
  menufolder: (id) => act(() => invoke("open_folder", { id })),
  menuresume: (id) => act(() => invoke("resume", { id })),
  menustop: (id) => act(() => invoke("pause", { id })),
  menucopy: (id) =>
    act(async () => {
      await navigator.clipboard.writeText(downloads.find((d) => d.id === id).url);
      setTimeout(() => (el("message").textContent = "Address copied"));
    }),
  // The row is the selection (a right click selects it): the usual question.
  menudelete: () => el("delete").click(),
  menuprops: (id) => act(() => invoke("open_properties", { id })),
};
for (const [item, run] of Object.entries(MENU)) {
  el(item).addEventListener("click", () => {
    hideRowMenu();
    run(menuFor);
  });
}
document.addEventListener("mousedown", (e) => {
  if (!el("rowmenu").contains(e.target)) hideRowMenu();
});
window.addEventListener("blur", hideRowMenu);

async function refresh() {
  downloads = await invoke("list");
  for (const id of [...selected]) if (!downloads.some((d) => d.id === id)) selected.delete(id);
  renderRows();
}

// Runs an action, shows its error in the status bar, then refreshes.
async function act(action) {
  el("message").textContent = "";
  try {
    await action();
  } catch (e) {
    el("message").textContent = String(e);
  }
  refresh();
}

function selectedDownloads() {
  return downloads.filter((d) => selected.has(d.id));
}

el("add").addEventListener("click", () => {
  el("addbar").hidden = false;
  el("newurl").value = "";
  el("newurl").focus();
});
el("addcancel").addEventListener("click", () => (el("addbar").hidden = true));
el("addok").addEventListener("click", () =>
  act(async () => {
    await invoke("add_url", { url: el("newurl").value });
    el("addbar").hidden = true;
  }),
);
el("newurl").addEventListener("keydown", (e) => {
  if (e.key === "Enter") el("addok").click();
  if (e.key === "Escape") el("addcancel").click();
});

el("resume").addEventListener("click", () =>
  act(async () => {
    for (const d of selectedDownloads()) if (d.status !== "complete" && d.status !== "downloading") await invoke("resume", { id: d.id });
  }),
);
el("stop").addEventListener("click", () =>
  act(async () => {
    for (const d of selectedDownloads()) if (d.status === "downloading") await invoke("pause", { id: d.id });
  }),
);
el("stopall").addEventListener("click", () => act(() => invoke("stop_all")));
el("options").addEventListener("click", () => act(() => invoke("open_options")));
el("startq").addEventListener("click", () => act(() => invoke("start_queue", { id: 1 })));
el("stopq").addEventListener("click", () => act(() => invoke("stop_queue", { id: 1 })));

// Asked in a window of its own (delete.html), which removes what is selected now.
el("delete").addEventListener("click", () => {
  const ids = selectedDownloads().map((d) => d.id);
  if (ids.length > 0) act(() => invoke("open_delete", { ids }));
});
document.addEventListener("keydown", (e) => {
  // Not while the key is held: every repeat would ask again. A Mac's delete key is Backspace.
  const deletes = e.key === "Delete" || (isMac && e.key === "Backspace");
  if (deletes && !e.repeat && document.activeElement?.tagName !== "INPUT") el("delete").click();
  if (e.key === "Escape") hideRowMenu();
});

(async () => {
  renderHeads();
  categories = await invoke("categories");
  renderTree();
  await refresh();
  setInterval(refresh, 700);
})();
