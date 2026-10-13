#!/bin/bash
# Verifies the downloaded sources (GPG, in a throwaway keyring under work/) and
# unpacks them into build/. Run inside WSL: bash verify_and_unpack.sh <ffmpeg-min dir>
set -euo pipefail
ROOT=$1
D=$ROOT/downloads
B=$ROOT/build
export GNUPGHOME=$ROOT/work/gnupg
rm -rf "$GNUPGHOME"; mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"

echo "== ffmpeg"
gpg -q --import "$D/ffmpeg/ffmpeg-devel.asc" 2>&1
gpg --with-colons --fingerprint | awk -F: '/^fpr/{print "key fingerprint:", $10}'
gpg --status-fd 1 --verify "$D/ffmpeg/ffmpeg-9.0.2.tar.xz.asc" "$D/ffmpeg/ffmpeg-9.0.2.tar.xz" 2>/dev/null | grep -E 'GOODSIG|VALIDSIG|BADSIG'

echo "== make"
gpg --status-fd 1 --no-default-keyring --keyring "$D/make/gnu-keyring.gpg" --verify "$D/make/make-4.4.1.tar.gz.sig" "$D/make/make-4.4.1.tar.gz" 2>/dev/null | grep -E 'GOODSIG|VALIDSIG|BADSIG'

echo "== zig sha256 (index.json: 70e49664...)"
sha256sum "$D/zig/zig-x86_64-linux-0.16.0.tar.xz"

mkdir -p "$B"
cd "$B"
[ -d ffmpeg-9.0.2 ] || tar xf "$D/ffmpeg/ffmpeg-9.0.2.tar.xz"
[ -d make-4.4.1 ] || tar xf "$D/make/make-4.4.1.tar.gz"
[ -d zig-x86_64-linux-0.16.0 ] || tar xf "$D/zig/zig-x86_64-linux-0.16.0.tar.xz"
ls "$B"
