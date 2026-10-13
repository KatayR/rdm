#!/bin/bash
# Builds RDM's app for the Mac and what an update needs beside it, in target/release/bundle:
#   dmg/RDM_<version>_<arch>.dmg                the disk image a first install is made from
#   macos/RDM_<version>_<arch>.app.tar.gz       the app as an update fetches it
#   macos/RDM_<version>_<arch>.app.tar.gz.sig   its signature, made with RDM's update key
#   macos/latest.json                           what an installed RDM reads to find this version
#
#   ./package.sh                                   as it is released
#   ./package.sh --notes "What is new"             with the text the update window shows
#   ./package.sh --latest <latest.json>            adds this Mac to the latest.json package.ps1
#                                                  wrote, so one file serves both systems
#   ./package.sh --config <file> --url http://..   a build to try an update with (see README,
#                                                  Making a release)
#
# The Mac counterpart of package.ps1, with the same needs: Tauri's command line
# (cargo install tauri-cli --version "^2" --locked), cargo-about (cargo install cargo-about
# --locked --features cli), the ffmpeg built by the scripts in ffmpeg/ for this Mac, and the
# update key (~/.tauri/rdm-update.key, or TAURI_SIGNING_PRIVATE_KEY). It builds for the
# kind of Mac it runs on.
set -euo pipefail
root=$(cd "$(dirname "$0")" && pwd)

notes=; url=; latest=; configs=()
while [ $# -gt 0 ]; do
    case $1 in
        # Shown in the update window of an RDM that finds this version.
        --notes) notes=$2 ;;
        # Where the app will be fetched from; the default is the release page of its version.
        --url) url=$2 ;;
        # A latest.json to add this Mac to (the file itself is left as it is).
        --latest) latest=$(cd "$(dirname "$2")" && pwd)/$(basename "$2") ;;
        # More configuration files, merged in after tauri.bundle.macos.json. Named from
        # where the script was called, before it moves into the repository.
        --config) configs+=("$(cd "$(dirname "$2")" && pwd)/$(basename "$2")") ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift 2
done

# The names Tauri gives the two kinds of Mac: in the disk image's name, and in latest.json.
case $(uname -m) in
    arm64)  arch=aarch64; platform=darwin-aarch64; ffmpeg=$root/target/ffmpeg-min/work/ffmpeg-macos-arm64/ffmpeg ;;
    x86_64) arch=x64;     platform=darwin-x86_64;  ffmpeg=$root/target/ffmpeg-min/work/ffmpeg-macos-x64/ffmpeg ;;
    *) echo "no build for $(uname -m)" >&2; exit 1 ;;
esac
[ -f "$ffmpeg" ] || { echo "no $ffmpeg: build it first (ffmpeg/README.md)" >&2; exit 1; }
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    key=$HOME/.tauri/rdm-update.key
    [ -f "$key" ] || { echo "no update key at $key: without the key the installed RDMs were given, they take no update" >&2; exit 1; }
    export TAURI_SIGNING_PRIVATE_KEY=$key
fi

cd "$root"
# The app's bundle takes rdm and rdm-host as files, so they come first.
cargo build --release -p rdm

# The licences of the libraries in the programs, as one page (about.toml, about.hbs).
mkdir -p target/licenses
cargo about generate --workspace about.hbs -o target/licenses/libraries.html

out=$root/target/release/bundle
rm -rf "$out/macos" "$out/dmg"

# tauri.conf.json alone builds without the files above (tests, work on the code);
# tauri.bundle.macos.json adds what only the app bundle needs, and ffmpeg is the one of
# this kind of Mac.
merge=(--config tauri.bundle.macos.json --config "{\"bundle\":{\"macOS\":{\"files\":{\"MacOS/ffmpeg\":\"$ffmpeg\"}}}}")
for config in ${configs[@]+"${configs[@]}"}; do merge+=(--config "$config"); done
(cd app && cargo tauri build --ci "${merge[@]}")

app=$out/macos/RDM.app
# Every program in the app must carry a signature that holds (the bundle's own is made
# without a certificate): an Apple silicon Mac starts nothing else.
codesign --verify --deep --strict "$app"

dmg=$(ls "$out"/dmg/RDM_*_"$arch".dmg)
version=$(basename "$dmg" | cut -d_ -f2)
# The update's file is named like the disk image: a release holds one for each kind of Mac.
update=RDM_${version}_$arch.app.tar.gz
[ -f "$app.tar.gz.sig" ] || { echo "no signed update in $out/macos" >&2; exit 1; }
mv "$app.tar.gz" "$out/macos/$update"
mv "$app.tar.gz.sig" "$out/macos/$update.sig"
[ -n "$url" ] || url=https://github.com/KatayR/rdm/releases/download/v$version/$update

NOTES=$notes URL=$url LATEST=$latest PLATFORM=$platform VERSION=$version python3 - "$out/macos/$update.sig" "$out/macos/latest.json" \
    app/tauri.conf.json ${configs[@]+"${configs[@]}"} <<'PY'
import base64, datetime, json, os, sys

signature_file, out, *configs = sys.argv[1:]

# The id of the key in a minisign public key or signature, as Tauri writes them: base64 of
# a text whose second line is base64 of two bytes, the key's eight-byte id, and the rest.
def key_id(encoded):
    text = base64.b64decode(encoded.strip()).decode()
    return base64.b64decode(text.split("\n")[1].strip())[2:10].hex().upper()

signature = open(signature_file).read().strip()
# Signed with another key than the one the installed RDMs carry, the build only warns,
# and every one of them would refuse this update. The key they carry is the one in
# app/tauri.conf.json, unless a configuration file merged in after it names another.
for conf in configs:
    named = json.load(open(conf)).get("plugins", {}).get("updater", {}).get("pubkey")
    if named:
        public, named_in = named, conf
if key_id(signature) != key_id(public):
    sys.exit(f"the update was signed with key {key_id(signature)}, but RDM checks updates against key {key_id(public)} ({named_in})")

version = os.environ["VERSION"]
if os.environ["LATEST"]:
    latest = json.load(open(os.environ["LATEST"], encoding="utf-8-sig"))
    if latest["version"] != version:
        sys.exit(f"{os.environ['LATEST']} is of version {latest['version']}, and this build is {version}")
    if os.environ["NOTES"]:
        latest["notes"] = os.environ["NOTES"]
else:
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    latest = {"version": version, "notes": os.environ["NOTES"], "pub_date": now, "platforms": {}}
latest["platforms"][os.environ["PLATFORM"]] = {"signature": signature, "url": os.environ["URL"]}
with open(out, "w", encoding="utf-8") as f:
    json.dump(latest, f, indent=2, ensure_ascii=False)
    f.write("\n")
PY

ls -l "$dmg" "$out"/macos/RDM_* "$out/macos/latest.json"
