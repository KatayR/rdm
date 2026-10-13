#!/bin/bash
# Minimal, LGPL, stream-copy-only ffmpeg for RDM, cross-compiled with zig cc.
# Usage (inside WSL): bash build_ffmpeg.sh win64|macos-arm64|macos-x64|linux-x64 [extra configure flags]
# On a Mac the two macOS targets are built with Xcode's clang instead, without zig.
set -euo pipefail
. "$(dirname "$0")/env.sh"
TARGET=$1; shift
case $TARGET in
    win64)       ZT=x86_64-windows-gnu; OS=mingw32; ARCH=x86_64 ;;
    macos-arm64) ZT=aarch64-macos;      OS=darwin;  ARCH=aarch64 ;;
    macos-x64)   ZT=x86_64-macos;       OS=darwin;  ARCH=x86_64 ;;
    linux-x64)   ZT=x86_64-linux-musl;  OS=linux;   ARCH=x86_64 ;;
    *) echo "unknown target $TARGET"; exit 2 ;;
esac
MAC=; [ "$(uname)" = Darwin ] && MAC=1
if [ -n "$MAC" ] && [ $OS != darwin ]; then echo "on a Mac only macos-arm64 and macos-x64 are built"; exit 2; fi
OUT=$ROOT/work/ffmpeg-$TARGET${SUFFIX:-}
# zig cannot rename its macOS runtime builds inside a cache on the Windows drive (drvfs,
# AccessDenied every time); those targets get a throwaway cache in WSL's /tmp, removed at the end.
if [ $OS = darwin ] && [ -z "$MAC" ]; then
    export ZIG_GLOBAL_CACHE_DIR=/tmp/rdm-ffmpeg-min-zig-cache ZIG_LOCAL_CACHE_DIR=/tmp/rdm-ffmpeg-min-zig-cache
    trap 'rm -rf /tmp/rdm-ffmpeg-min-zig-cache' EXIT
fi
rm -rf "$OUT"; mkdir -p "$OUT"; cd "$OUT"

# What RDM's media.rs does: -i <mp4|m4a|webm|mkv|ts|aac...> -map ... -c copy
# [-movflags +faststart] -f mp4|webm|matroska <file>. Nothing is decoded or encoded.
FLAGS=(
    --disable-everything --disable-autodetect --disable-network --disable-doc
    --disable-programs --enable-ffmpeg
    --disable-avdevice --disable-swscale --disable-swresample
    --disable-asm --disable-debug --enable-small
    --enable-static --disable-shared
    # Inputs: (fragmented) MP4/M4A, WebM/Matroska, MPEG-TS, HLS packed audio.
    --enable-demuxer=mov,matroska,mpegts,aac,ac3,eac3,mp3
    # Outputs.
    --enable-muxer=mp4,webm,matroska
    # Parsers the demuxers above ask for (TS needs them to cut PES into frames; MP4 and
    # Matroska use them for headers). Missing ones would change packet boundaries.
    --enable-parser=h264,hevc,vvc,av1,vp8,vp9,mpeg4video,mpegvideo,aac,aac_latm,ac3,mpegaudio,opus,vorbis,flac,dca,mlp
    # aac_adtstoasc + vp9_superframe (+ pgs_frame_merge) come with the muxers;
    # extract_extradata gives SPS/PPS etc. of TS streams to the MP4 header.
    --enable-bsf=aac_adtstoasc,vp9_superframe,extract_extradata
    --enable-protocol=file
    # Decoders only for probing (avformat_find_stream_info), nothing is decoded into the output:
    # - aac: the AAC parser leaves sample rate and channels unset on purpose (HE-AAC), and
    #   MP4/Matroska refuse a stream without them (TS / ADTS input fails without it).
    # - vp9: MP4 keeps VP9's bit depth and chroma subsampling in a vpcC box, which the muxer
    #   builds from the stream's pixel format. Only the decoder reads that from the frame
    #   header (the vp9 parser gives the profile alone; the vp9_metadata filter gives
    #   nothing to the muxer). Without it VP9 -> MP4 (YouTube's HLS pieces) gets an empty
    #   vpcC box, no ffmpeg can open the file, and the exit code is still 0.
    # - h264, hevc: colour description and aspect ratio of TS streams (MP4 colr / pasp
    #   boxes). Without them the output still plays (the values stay in the SPS) but its
    #   header differs from the full ffmpeg's; with them every MP4 is byte-identical.
    #   Set DECODERS=aac,vp9 for the smallest build.
    --enable-decoder=${DECODERS:-aac,h264,hevc,vp9}
)
case $OS in
    mingw32) FLAGS+=(--disable-pthreads --enable-w32threads) ;;
esac

if [ -n "$MAC" ]; then
    # The oldest macOS RDM's own programs run on, for each kind of Mac.
    case $ARCH in aarch64) CA=arm64; MIN=11.0 ;; *) CA=x86_64; MIN=10.13 ;; esac
    TOOLS=(--cc="clang -arch $CA -mmacosx-version-min=$MIN")
    # The other kind of Mac than this one: configure cannot run what it builds.
    [ "$(uname -m)" = $CA ] || TOOLS+=(--enable-cross-compile --target-os=$OS --arch=$ARCH --host-cc=clang)
    JOBS=$(sysctl -n hw.ncpu)
else
    TOOLS=(
        --enable-cross-compile --target-os=$OS --arch=$ARCH
        --cc="zigcc -target $ZT" --ld="zigcc -target $ZT"
        --host-cc="zig cc" --ar="zig ar" --ranlib="zig ranlib" --nm=true --strip=true
        --extra-ldflags="-s"
    )
    JOBS=$(nproc)
fi

"$B/ffmpeg-9.0.2/configure" \
    "${TOOLS[@]}" \
    --pkg-config=false \
    "${FLAGS[@]}" "$@" > configure.out 2>&1 || { tail -30 configure.out; tail -40 ffbuild/config.log; exit 1; }

grep -E '^(Enabled|External|License)' -A3 configure.out | head -60
# make 4.4's default jobserver uses a fifo, which the Windows drive under WSL cannot hold.
JS=; make --version | grep -q 'Make 4\.[4-9]' && JS=--jobserver-style=pipe
make -j"$JOBS" $JS > make.out 2>&1 || { grep -E 'error|Error' make.out | head -40; exit 1; }
if [ -n "$MAC" ]; then
    # The Makefile stripped ffmpeg_g into ffmpeg, which leaves the linker's signature stale,
    # and an Apple silicon Mac runs nothing without a valid one.
    codesign --force --sign - "$OUT/ffmpeg"
else
    # Linked with -s already; the Makefile only copies ffmpeg_g to ffmpeg when it strips.
    EXE=; [ $OS = mingw32 ] && EXE=.exe
    cp "$OUT/ffmpeg_g$EXE" "$OUT/ffmpeg$EXE"
fi
ls -l "$OUT"/ffmpeg*
