# Sourced by the build scripts (inside WSL). Sources, tools and builds go to ROOT
# (default: target/ffmpeg-min in the project, which git ignores); zig's caches too, so
# nothing lands in the WSL home.
SCRIPTS=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=${ROOT:-$(cd "$SCRIPTS/.." && pwd)/target/ffmpeg-min}
B=$ROOT/build
ZIG=${ZIG:-$B/zig-x86_64-linux-0.16.0}
export ZIG_GLOBAL_CACHE_DIR=$ROOT/work/zig-cache
export ZIG_LOCAL_CACHE_DIR=$ROOT/work/zig-cache
export PATH=$SCRIPTS:$ROOT/work/bin:$ZIG:$PATH
export TMPDIR=$ROOT/work/tmp; mkdir -p $TMPDIR
