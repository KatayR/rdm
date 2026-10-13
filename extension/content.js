// Link clicks, before the browser acts on them.
//
// A plain left click on a link that looks like a captured file goes to RDM directly.
// That also covers http links on https pages, which the browser blocks as insecure
// downloads before extensions see them. If RDM does not take the link, the click is
// replayed and the browser handles it as usual.
//
// Alt+click downloads a link with the browser itself (Chrome's own shortcut), so RDM
// lets that one through.

const REPLAYED = "rdmReplayed";

document.addEventListener(
  "click",
  (event) => {
    const link = event.target instanceof Element ? event.target.closest("a[href]") : null;
    if (!link) return;
    if (link.dataset[REPLAYED]) {
      delete link.dataset[REPLAYED];
      return;
    }
    if (event.altKey) {
      chrome.runtime.sendMessage({ skip: link.href });
      return;
    }
    if (event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.defaultPrevented) return;
    let url;
    try {
      url = new URL(link.href);
    } catch {
      return;
    }
    if (!/^https?:$/.test(url.protocol) || !rdmCaptured(decodeURIComponent(url.pathname))) return;

    // After the extension was loaded anew (an update of RDM), this script of the copy
    // before is still in the page and reaches nobody. The click is then the browser's:
    // the new copy sees the download it starts.
    if (!chrome.runtime?.id) return;
    event.preventDefault();
    chrome.runtime.sendMessage({ take: link.href, referrer: location.href }).then(
      (reply) => reply?.taken || replay(link),
      () => replay(link),
    );
  },
  true,
);

function replay(link) {
  link.dataset[REPLAYED] = "1";
  link.click();
}
