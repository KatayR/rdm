// Download Video: the qualities of a video page (yt-dlp) and of every stream its browser
// tab played, one section per source. The program sorts out what the tab played and lists
// the playlists whose text the page read; yt-dlp is asked about the others. Nothing is
// chosen until the person picks a line, as in IDM's list.

const token = Number(param("token"));
let pending = null;
let page = null;
// Sections as they arrive: { title, note, order, choices: [{ ...choice, src }] }. `src` is
// null for the page itself, else what the tab played (its `index` names it in the answer).
// A stream's section also has its `source`, the page's the `manifests` its list comes from.
// The page's section is `ahead` while it shows what the page's own player was given and
// yt-dlp's list is not in yet.
const sections = [];
let fileStem = "video";
let titled = false;
let chosen = null;
let categories = [];
let pathEdited = false;
let answered = false;
// The page's own lookup (yt-dlp), once it runs.
let pageLookup = null;

const HINT =
  "If the video played before the RDM extension was loaded, reload the page, play the video, then try again.";

async function load() {
  pending = await invoke("pending", { token });
  page = await invoke("video_page", { token });
  el("url").value = page.url;
  if (page.title) {
    setTitle(page.title);
    fileStem = stemOf(page.title);
  }
  categories = await invoke("categories");
  for (const c of categories) el("category").add(new Option(c.name, c.name));
  for (const q of await invoke("queues")) el("queue").add(new Option(q.name, q.id));

  // What the page's player was given is there at once; yt-dlp's list takes its place.
  const ahead = await invoke("video_ahead", { token });
  if (ahead) {
    setTitle(ahead.title);
    fileStem = ahead.file_stem;
    addSection({ url: page.url }, ahead, true);
    sections.at(-1).ahead = true;
  }

  const played = await invoke("video_sources", { token });
  for (const earlier of [false, true]) {
    const files = played.filter((c) => c.kind === "file" && c.earlier === earlier);
    if (files.length) {
      sections.push({ title: named("Played files", earlier), note: "", order: order(2, earlier), choices: files.map(fileChoice) });
    }
  }
  const playlists = played.filter((c) => c.kind !== "file");
  for (const c of playlists.filter((c) => c.info)) addSection(c, c.info, false);
  const asked = playlists.filter((c) => !c.info);
  pageLookup = lookup({ url: page.url, headers: page.headers }, true);
  const lookups = [pageLookup, ...asked.map((c) => lookup(c, false))];
  renderChoices();
  if (sections.length && !titled) el("title").textContent = "Choose a quality";
  const errors = (await Promise.all(lookups)).filter(Boolean);
  if (!sections.length) {
    el("title").textContent = "No video found";
    el("error").textContent = errors[0] || "";
    el("hint").textContent = HINT;
  } else if (!chosen) {
    await placeFile(`${fileStem}.mp4`);
  }
}

// Asks for one source's qualities and adds its section; returns the error text, if any.
async function lookup(source, isPage) {
  try {
    const info = await invoke("video_choices", { page: source.url, headers: source.headers || [] });
    let picked = null;
    if (isPage) {
      setTitle(info.title);
      fileStem = info.file_stem;
      picked = dropAhead();
    }
    addSection(source, info, isPage);
    if (picked) await pickAgain(picked, sections.at(-1).choices);
    renderChoices();
    if (!titled) el("title").textContent = "Choose a quality";
    return null;
  } catch (e) {
    // Without yt-dlp's list the page's own cannot be started: it goes, and a line picked
    // from it with it.
    if (isPage && dropAhead()) {
      chosen = null;
      el("start").disabled = true;
      el("later").disabled = true;
      el("error").textContent = String(e);
    }
    if (isPage) renderChoices();
    return String(e);
  }
}

// One source's qualities: the page's, or those of a stream the tab played.
function addSection(source, info, isPage) {
  const earlier = !isPage && source.earlier;
  sections.push({
    title: isPage ? "This page" : named(`${source.kind.toUpperCase()} stream`, earlier),
    note: [isPage ? "" : hostOf(source.url), duration(info.duration)].filter(Boolean).join(" · "),
    order: isPage ? 0 : order(1, earlier),
    length: info.duration || 0,
    choices: info.choices.map((c) => ({ ...c, src: isPage ? null : source })),
    source: isPage ? null : source,
    manifests: isPage ? (info.manifests || []).map(plain) : null,
  });
}

// yt-dlp names the playlist each of its formats comes from. When the page's whole list
// comes from playlists the tab played, it is those streams once more and is left out: the
// streams stay, with what the browser sent for them and, when the page read them, their
// text. Not when the lengths differ (some sites serve every video's playlist from one
// address), not for a stream from before the tab moved on, and not once a line of the
// page's list was picked.
function dropPageCopy() {
  const page = sections.find((s) => s.manifests);
  if (!page || !page.manifests.length || page.choices.includes(chosen)) return;
  // Lengths as two readers of one playlist give them: a few seconds apart at most.
  const alike = (a, b) => !a || !b || Math.abs(a - b) <= Math.max(2, 0.02 * Math.max(a, b));
  const played = (address) =>
    sections.some((s) => s.source && !s.source.earlier && plain(s.source.url) === address && alike(s.length, page.length));
  if (page.manifests.every(played)) sections.splice(sections.indexOf(page), 1);
}

// An address without its fragment.
function plain(url) {
  try {
    const u = new URL(url);
    u.hash = "";
    return u.href;
  } catch {
    return url;
  }
}

// Takes the list made from the page's own data out, now that yt-dlp's is in. Returns the
// line that was picked from it, if one was.
function dropAhead() {
  const i = sections.findIndex((s) => s.ahead);
  if (i < 0) return null;
  const [old] = sections.splice(i, 1);
  return old.choices.includes(chosen) ? chosen : null;
}

// The picked line in yt-dlp's list: the same formats, else the line that looks the same,
// else the one of the same picture size with the same picture codec (yt-dlp often names
// no sound codec, or gets the sound another way), else the best of that size (yt-dlp may
// get it in another codec, or as a playlist, than the page's player), in the same
// container if there is one. When there is none the pick is gone and has to be made again.
async function pickAgain(picked, choices) {
  const picture = (c) => c.codecs.split(" + ")[0];
  const same = [
    (c) => c.format === picked.format,
    (c) => c.ext === picked.ext && c.label === picked.label && c.codecs === picked.codecs,
    (c) => c.ext === picked.ext && c.label === picked.label && picture(c) === picture(picked),
    (c) => c.ext === picked.ext && c.label === picked.label,
    (c) => c.ext === picked.ext && c.resolution === picked.resolution,
    (c) => c.resolution === picked.resolution,
  ];
  chosen = same.map((test) => choices.find(test)).find(Boolean) || null;
  if (!chosen) {
    el("start").disabled = true;
    el("later").disabled = true;
    el("error").textContent = "The list changed: choose a quality again.";
  } else if (chosen.ext !== picked.ext) {
    await fitPath(chosen);
  }
}

// What the tab played before a single-page site moved on is listed last, and says so.
function named(title, earlier) {
  return earlier ? `${title}, earlier in this tab` : title;
}

function order(place, earlier) {
  return earlier ? place + 10 : place;
}

function setTitle(title) {
  titled = true;
  el("title").textContent = title;
  el("title").title = title;
  thisWindow.setTitle(`Download Video · ${title}`);
}

// A tab title as a file name (the page's own title comes from yt-dlp already cleaned).
function stemOf(title) {
  const stem = title.replace(/[<>:"/\\|?*\u0000-\u001f]/g, "_").replace(/[. ]+$/, "").trim().slice(0, 150);
  return stem || "video";
}

function hostOf(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

function duration(seconds) {
  if (!seconds) return "";
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

// A media file the tab played, offered as it is.
function fileChoice(c) {
  let name = "file";
  try {
    name = decodeURIComponent(new URL(c.url).pathname.split("/").pop()) || name;
  } catch {}
  const dot = name.lastIndexOf(".");
  const ext = dot > 0 ? name.slice(dot + 1).toLowerCase() : "mp4";
  return { format: null, ext, label: name, resolution: undefined, codecs: hostOf(c.url), size: c.size, approx: false, stream: null, src: c };
}

const RESOLUTION_NAMES = { 4320: "8K", 2160: "4K", 1440: "2K", 1080: "Full HD", 720: "HD" };

function groupName(resolution) {
  if (resolution === undefined) return null;
  if (resolution == null) return "Audio only";
  if (resolution === 0) return "Picture size not named";
  const name = RESOLUTION_NAMES[resolution];
  return name ? `${resolution}p · ${name}` : `${resolution}p`;
}

function headerRow(cls, text, note) {
  const tr = document.createElement("tr");
  tr.className = cls;
  const cell = document.createElement("td");
  cell.colSpan = 3;
  cell.textContent = text;
  if (note) {
    const span = document.createElement("span");
    span.className = "muted";
    span.textContent = note;
    cell.append(span);
  }
  tr.append(cell);
  return tr;
}

function chip(cls, text) {
  const span = document.createElement("span");
  span.className = `chip ${cls}`;
  span.textContent = text;
  return span;
}

// The page first, then the streams the tab played, longest first (ads are short), then
// plain files. Inside a section, rows are grouped under a header per resolution and a
// colored chip shows each row's format. Each row is a radio button, so the list also
// works with the keyboard (arrow keys).
function renderChoices() {
  const body = el("choices");
  body.textContent = "";
  dropPageCopy();
  sections.sort((a, b) => a.order - b.order || (b.length || 0) - (a.length || 0));
  const several = sections.length > 1;
  for (const section of sections) {
    if (several || section.order !== 0) body.append(headerRow("source", section.title, section.note));
    let group = null;
    for (const c of section.choices) {
      const name = groupName(c.resolution);
      if (name && name !== group) {
        group = name;
        body.append(headerRow("resolution", name));
      }
      body.append(choiceRow(c));
    }
  }
}

function choiceRow(c) {
  const tr = document.createElement("tr");
  tr.className = "choice";
  if (c === chosen) tr.classList.add("selected");
  const td = (text, cls) => {
    const cell = document.createElement("td");
    cell.textContent = text;
    if (cls) cell.className = cls;
    return cell;
  };
  const first = document.createElement("td");
  const label = document.createElement("label");
  label.className = "check";
  const radio = document.createElement("input");
  radio.type = "radio";
  radio.name = "choice";
  radio.checked = c === chosen;
  radio.addEventListener("change", () => choose(c, tr));
  label.append(radio, chip(c.ext, c.ext.toUpperCase()));
  if (c.stream) label.append(chip("stream", c.stream));
  label.append(c.label);
  first.append(label);
  const size = c.size != null ? `${c.approx ? "~" : ""}${human(c.size)}` : "";
  tr.append(first, td(c.codecs), td(size, "num"));
  tr.addEventListener("click", (e) => {
    if (e.target !== radio && !radio.checked) radio.click();
  });
  tr.addEventListener("dblclick", () => answer(true));
  return tr;
}

async function choose(c, row) {
  chosen = c;
  for (const tr of el("choices").querySelectorAll("tr.choice")) tr.classList.toggle("selected", tr === row);
  el("start").disabled = false;
  el("later").disabled = false;
  el("error").textContent = "";
  await fitPath(c);
}

// Save As for the chosen line: its extension on the person's own folder and name, or the
// video's name in the folder of its category.
async function fitPath(c) {
  if (pathEdited) {
    const [dir, name] = splitPath(el("path").value);
    const stem = name.includes(".") ? name.slice(0, name.lastIndexOf(".")) : name;
    el("path").value = joinPath(dir, `${stem}.${c.ext}`);
  } else {
    await placeFile(`${fileStem}.${c.ext}`);
  }
}

// Picks the category by file name and puts the file in its folder.
async function placeFile(name) {
  const category = await invoke("category_for", { fileName: name });
  if (category) el("category").value = category.name;
  el("path").value = joinPath(pending.dir || category?.dir || "", name);
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
  if (!chosen) {
    el("error").textContent = "Choose a quality first.";
    return;
  }
  el("error").textContent = "";
  // A line of the page's own list is named as the page names its formats. yt-dlp's names
  // can differ (a video in several languages), so its list is waited for and the same line
  // taken from it.
  if (sections.some((s) => s.ahead && s.choices.includes(chosen))) {
    el("start").disabled = true;
    el("later").disabled = true;
    el("hint").textContent = "Getting the video's addresses…";
    await pageLookup;
    el("hint").textContent = "";
    if (!chosen) return;
    el("start").disabled = false;
    el("later").disabled = false;
  }
  try {
    await invoke("confirm", {
      token,
      start,
      path: el("path").value.trim(),
      description: el("description").value,
      queue: start ? null : Number(el("queue").value),
      pick: { format: chosen.format, caught: chosen.src ? chosen.src.index : null },
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

// Closing the window in any way drops a download that was not answered.
thisWindow.onCloseRequested(async () => {
  if (!answered) await invoke("cancel_pending", { token });
});

load().catch((e) => {
  el("title").textContent = "No video found";
  el("error").textContent = String(e);
});
