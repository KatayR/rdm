// The question before downloads are removed (Delete in the main window): from the list
// only, or with their files. Enter removes from the list (files stay), Escape cancels.

const WIDTH = 460;
// Names shown before "and N more".
const SHOWN = 8;
const asked = (param("ids") ?? "").split(",").map(Number);
// Still to remove: what was asked for and is in the list, as the window names it.
let left = [];

// Names what is left to remove; false when nothing is.
async function show() {
  const found = (await invoke("list")).filter((d) => asked.includes(d.id));
  left = found.map((d) => d.id);
  const [one, none] = [found.length === 1, found.length === 0];
  el("question").textContent = none ? "Nothing left to remove" : one ? "Remove this download?" : `Remove these ${found.length} downloads?`;
  const line = (text, className = "") => Object.assign(document.createElement("div"), { textContent: text, title: text, className });
  el("names").replaceChildren(...found.slice(0, SHOWN).map((d) => line(fileNameOf(d))));
  if (found.length > SHOWN) el("names").append(line(`and ${found.length - SHOWN} more`, "hint"));
  el("unfinished").hidden = found.every((d) => d.status === "complete");
  el("unfinished").textContent = one ? "It is not finished: what it has downloaded is deleted either way." : "What the unfinished ones have downloaded is deleted either way.";
  el("list").hidden = el("files").hidden = none;
  el("cancel").textContent = none ? "Close" : "Cancel";
  await fitWindow(WIDTH);
  return !none;
}

async function remove(deleteFile) {
  const buttons = ["list", "files", "cancel"].map(el);
  el("error").textContent = "";
  for (const b of buttons) b.disabled = true;
  try {
    for (const id of left) await invoke("remove", { id, deleteFile });
    await thisWindow.close();
  } catch (e) {
    el("error").textContent = String(e);
    // What went before the error, or with it (an entry goes before its file), is not
    // asked for again.
    await show().catch(() => {});
    for (const b of buttons) b.disabled = false;
    el(left.length > 0 ? "list" : "cancel").focus();
  }
}

el("list").addEventListener("click", () => remove(false));
el("files").addEventListener("click", () => remove(true));
el("cancel").addEventListener("click", () => thisWindow.close());

showWhenReady(async () => {
  // Removed meanwhile: nothing to ask.
  if (!(await show())) return thisWindow.close();
  el("list").focus();
});
