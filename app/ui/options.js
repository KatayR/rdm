// Options: edits a copy of the settings, folders and queues; OK saves what changed.

const MB = 1024 * 1024;
const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
let settings = null;
let categories = [];
const changedFolders = new Map();
let queues = [];
const changedQueues = new Set();
let shownQueue = null;

for (const tab of document.querySelectorAll(".tab")) {
  tab.addEventListener("click", () => {
    for (const t of document.querySelectorAll(".tab")) t.classList.toggle("selected", t === tab);
    for (const page of document.querySelectorAll(".page")) page.hidden = page.id !== tab.dataset.page;
  });
}

async function load() {
  settings = await invoke("settings");
  el("ask").checked = settings.ask_before_download;
  el("progress").checked = settings.show_progress;
  el("complete").checked = settings.show_complete;
  el("exclude").value = settings.exclude_types.join(" ");
  el("connections").value = settings.connections;
  el("limit").checked = settings.speed_limit > 0;
  const rate = settings.speed_limit || settings.speed_limit_value;
  el("rate").value = rate ? +(rate / MB).toFixed(1) : 5;
  showLimit();

  categories = await invoke("categories");
  renderFolders();

  for (const [i, day] of DAYS.entries()) {
    const label = document.createElement("label");
    label.className = "check";
    label.innerHTML = `<input type="checkbox" data-day="${i}" /> ${day}`;
    el("days").append(label);
  }
  queues = await invoke("queues");
  renderQueueList(queues[0]?.id);

  el("autoupdate").checked = settings.auto_update_tools;
  renderTools(await invoke("tools"));

  el("checkupdates").checked = settings.check_for_updates;
  el("version").textContent = await invoke("app_version");
  if (isMac) el("licenses").textContent = 'the folder "Contents/Resources/licenses" inside the app (Show Package Contents)';
}

// About's button: asks the release page now; a newer version gets its own window.
el("checknow").addEventListener("click", async () => {
  el("checknow").disabled = true;
  el("checkstate").textContent = "Looking...";
  try {
    const found = await invoke("update_check");
    el("checkstate").textContent = found ? `Version ${found.version} is available.` : "This is the newest version.";
    if (found) await invoke("open_update");
  } catch (e) {
    el("checkstate").textContent = String(e);
  } finally {
    el("checknow").disabled = false;
  }
});

// Name, version and channel of each program; `results` (from Update now) beside them.
function renderTools(tools, results = []) {
  const table = el("toollist");
  table.textContent = "";
  for (const t of tools) {
    const row = table.insertRow();
    const name = document.createElement("th");
    name.textContent = t.name;
    row.append(name);
    const result = results.find((r) => r.name === t.name);
    const cell = row.insertCell();
    cell.textContent = result ? result.text : t.version ? `${t.version}${t.nightly ? " (nightly)" : ""}` : "not installed";
    cell.title = cell.textContent;
    if (result && !result.ok) cell.className = "error";
  }
}

el("updatenow").addEventListener("click", async () => {
  el("updatenow").disabled = true;
  el("updatestate").textContent = "Checking for new releases...";
  try {
    const results = await invoke("update_tools");
    renderTools(await invoke("tools"), results);
    el("updatestate").textContent = "";
  } catch (e) {
    el("updatestate").textContent = String(e);
  } finally {
    el("updatenow").disabled = false;
  }
});

function showLimit() {
  el("rate").disabled = !el("limit").checked;
}
el("limit").addEventListener("change", showLimit);

function renderFolders() {
  const box = el("folders");
  box.textContent = "";
  for (const c of categories) {
    const row = document.createElement("div");
    row.className = "row";
    const label = document.createElement("label");
    label.textContent = c.name;
    const input = document.createElement("input");
    input.value = changedFolders.get(c.name) ?? c.dir;
    input.spellcheck = false;
    input.addEventListener("input", () => changedFolders.set(c.name, input.value.trim()));
    const browse = document.createElement("button");
    browse.className = "small";
    browse.textContent = "…";
    browse.title = "Choose a folder";
    browse.addEventListener("click", async () => {
      const folder = await invoke("browse_folder", { start: input.value || null });
      if (folder) {
        input.value = folder;
        changedFolders.set(c.name, folder);
      }
    });
    row.append(label, input, browse);
    box.append(row);
  }
}

// Queue fields show one queue at a time; edits go back into `queues` before switching.
function renderQueueList(selectId) {
  el("queue").textContent = "";
  for (const q of queues) el("queue").add(new Option(q.name, q.id));
  el("queue").value = selectId;
  showQueue(queues.find((q) => q.id === selectId));
}

function showQueue(q) {
  shownQueue = q;
  el("qname").value = q.name;
  el("parallel").value = q.parallel;
  el("retries").value = q.retries;
  el("startOn").checked = !!q.schedule.start;
  el("startAt").value = q.schedule.start || "02:00";
  el("stopOn").checked = !!q.schedule.stop;
  el("stopAt").value = q.schedule.stop || "07:00";
  for (const box of el("days").querySelectorAll("input")) {
    box.checked = q.schedule.days === 0 || (q.schedule.days & (1 << Number(box.dataset.day))) !== 0;
  }
}

function keepQueueEdits() {
  const q = shownQueue;
  if (!q) return;
  let days = 0;
  for (const box of el("days").querySelectorAll("input")) if (box.checked) days |= 1 << Number(box.dataset.day);
  const edited = {
    ...q,
    name: el("qname").value.trim() || q.name,
    parallel: Math.max(1, Number(el("parallel").value) || 1),
    retries: Math.max(0, Number(el("retries").value) || 0),
    schedule: {
      start: el("startOn").checked ? el("startAt").value : null,
      stop: el("stopOn").checked ? el("stopAt").value : null,
      // Every day selected is stored as 0, the same as no choice.
      days: days === 0x7f ? 0 : days,
    },
  };
  if (JSON.stringify(edited) !== JSON.stringify(q)) {
    Object.assign(q, edited);
    changedQueues.add(q.id);
  }
}

el("queue").addEventListener("change", () => {
  keepQueueEdits();
  showQueue(queues.find((q) => q.id === Number(el("queue").value)));
});

el("newqueue").addEventListener("click", () => {
  el("newbar").hidden = false;
  el("newname").focus();
});
el("newok").addEventListener("click", async () => {
  const name = el("newname").value.trim();
  if (!name) return;
  try {
    keepQueueEdits();
    const q = await invoke("new_queue", { name });
    queues.push(q);
    el("newbar").hidden = true;
    el("newname").value = "";
    renderQueueList(q.id);
  } catch (e) {
    el("error").textContent = String(e);
  }
});

el("ok").addEventListener("click", async () => {
  el("error").textContent = "";
  keepQueueEdits();
  const rate = Math.round(Number(el("rate").value) * MB);
  if (el("limit").checked && !(rate > 0)) {
    el("error").textContent = "Enter a speed limit above 0, or switch the limit off.";
    return;
  }
  const updated = {
    ...settings,
    ask_before_download: el("ask").checked,
    show_progress: el("progress").checked,
    show_complete: el("complete").checked,
    exclude_types: el("exclude").value.toLowerCase().split(/[\s,;]+/).map((t) => t.replace(/^\./, "")).filter(Boolean),
    connections: Number(el("connections").value),
    speed_limit: el("limit").checked ? rate : 0,
    speed_limit_value: rate > 0 ? rate : settings.speed_limit_value,
    auto_update_tools: el("autoupdate").checked,
    check_for_updates: el("checkupdates").checked,
  };
  try {
    await invoke("update_settings", { settings: updated });
    for (const [name, dir] of changedFolders) await invoke("set_category_dir", { name, dir });
    for (const id of changedQueues) await invoke("update_queue", { queue: queues.find((q) => q.id === id) });
    await thisWindow.close();
  } catch (e) {
    el("error").textContent = String(e);
  }
});
el("cancel").addEventListener("click", () => thisWindow.close());

load().catch((e) => (el("error").textContent = String(e)));
