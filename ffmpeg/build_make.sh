#!/bin/bash
# GNU make from its signed source, built with zig cc via make's own build.sh (no make needed).
set -euo pipefail
. "$(dirname "$0")/env.sh"
mkdir -p "$ROOT/work/bin" "$ROOT/work/make-build"
cd "$ROOT/work/make-build"
CC="zig cc" "$B/make-4.4.1/configure" --disable-nls --disable-dependency-tracking -q
./build.sh
cp make "$ROOT/work/bin/make"
"$ROOT/work/bin/make" --version | head -1
