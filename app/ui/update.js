// A newer RDM was found: what it is, and Update now or Later. Update now fetches the
// setup (its signature is checked), closes RDM down and runs it; RDM starts again when
// the setup is done. Enter updates, Escape is Later.

const WIDTH = 460;

function showProgress([received, size]) {
  el("progress").textContent =
    size > 0 && received >= size
      ? "The update is here. RDM closes for it now and starts again by itself."
      : size > 0
        ? `Downloading the update: ${human(received)} of ${human(size)}`
        : `Downloading the update: ${human(received)}`;
}

// The update is being fetched: its progress is shown, and nothing can be pressed (it goes
// on when this window is closed, and RDM closes for it when it is here).
async function watch() {
  for (const b of ["now", "cancel"].map(el)) b.disabled = true;
  el("progress").hidden = false;
  showProgress(await invoke("update_progress"));
  await fitWindow(WIDTH);
  return setInterval(async () => showProgress(await invoke("update_progress")), 300);
}

async function update() {
  el("error").textContent = "";
  const watching = await watch();
  try {
    // Does not come back when it works: RDM closes for its setup.
    await invoke("update_install");
  } catch (e) {
    el("error").textContent = `The update was not installed: ${e}`;
    el("progress").hidden = true;
    for (const b of ["now", "cancel"].map(el)) b.disabled = false;
    el("now").focus();
    await fitWindow(WIDTH);
  } finally {
    clearInterval(watching);
  }
}

el("now").addEventListener("click", update);
el("cancel").addEventListener("click", () => thisWindow.close());

showWhenReady(async () => {
  const found = await invoke("update_found");
  if (!found) return thisWindow.close();
  el("question").textContent = `RDM ${found.version} is available`;
  el("current").textContent = `This is version ${found.current}.`;
  el("notes").textContent = found.notes.trim();
  el("notes").hidden = !found.notes.trim();
  el("running").hidden = found.running === 0;
  el("running").textContent =
    found.running === 1
      ? "One download is running: it stops for the update and continues after it."
      : `${found.running} downloads are running: they stop for the update and continue after it.`;
  // Opened again while Update now is at work.
  if (found.installing) return void (await watch());
  await fitWindow(WIDTH);
  el("now").focus();
});
