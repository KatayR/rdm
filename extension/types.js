// Link addresses that look like files: the default type list; `*` works as in IDM.
// Only used to catch link clicks before the browser acts (content.js); downloads the
// browser starts are all taken, whatever the type.

const RDM_TYPES =
  "3gp 7z aac ace aif apk arj asf avi bin bz2 doc docx exe flv gz gzip img iso lzh m4a m4v mkv mov mp3 " +
  "mp4 mpa mpe mpeg mpg msi msu ogg ogv pdf plj pps ppt qt r0* r1* ra rar rm rmvb sea sit sitx tar tif " +
  "tiff wav webm wma wmv z zip";

const RDM_CAPTURED = new RegExp(
  "^(?:" + RDM_TYPES.split(" ").map((t) => t.replace("*", ".*")).join("|") + ")$",
  "i",
);

// True when the file name's extension is on the list.
function rdmCaptured(fileName) {
  const base = fileName.split(/[\\/]/).pop();
  const dot = base.lastIndexOf(".");
  return dot > 0 && RDM_CAPTURED.test(base.slice(dot + 1));
}
