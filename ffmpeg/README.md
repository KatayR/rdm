# Minimal ffmpeg for RDM

RDM uses ffmpeg only to copy streams into a container (`manager/src/media.rs`: joining
video + audio, and turning HLS / DASH pieces into a normal file), never to encode. This
folder builds an ffmpeg with exactly that from the official source: one 4.2 MB program
instead of the 157 MB LGPL build from BtbN. It has the H.264 / HEVC decoders, so every MP4
is byte-identical to the full ffmpeg's, and the VP9 decoder (3.6 MB became 4.2 MB), because
VP9 into MP4 came out unreadable without it (see "What needs a decoder"). The win64 build
ships beside RDM's programs.

## What is in it

LGPL 2.1+ (no `--enable-gpl`, no `--enable-nonfree`). See `build_ffmpeg.sh` for the exact
flags and why each part is there:

- demuxers: mov (MP4 / M4A / fragmented MP4), matroska (MKV / WebM), mpegts, aac, ac3, eac3, mp3
- muxers: mp4, webm, matroska
- parsers for H.264, HEVC, VVC, AV1, VP8, VP9, MPEG-4, MPEG-2, AAC, AC-3, MP3, Opus, Vorbis,
  FLAC, DTS, MLP; bitstream filters aac_adtstoasc, vp9_superframe, extract_extradata
  (and vp9_superframe_split, which the VP9 decoder brings)
- decoders aac (required: the AAC parser leaves sample rate and channels unset), vp9
  (required: MP4 needs VP9's pixel format, which the VP9 parser does not give) and
  h264 / hevc (only read for colour and aspect information); nothing is decoded into a file
- the file protocol only

## Building

Cross-compiled inside WSL (Ubuntu) with portable zig; nothing is installed. Targets:
`win64`, `macos-arm64`, `macos-x64`, `linux-x64`. Everything goes to `target/ffmpeg-min`.

1. Download into `target/ffmpeg-min/downloads/` and check before use:
   - `ffmpeg/`: https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz and its `.asc`, and
     https://ffmpeg.org/ffmpeg-devel.asc (release key FCF986EA15E6E293A5644F10B4322F04D67658D8)
   - `make/`: https://ftp.gnu.org/gnu/make/make-4.4.1.tar.gz and its `.sig`, and
     https://ftp.gnu.org/gnu/gnu-keyring.gpg
   - `zig/`: https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz; compare its
     SHA-256 with https://ziglang.org/download/index.json and its minisign signature
     (`.minisig`) with `minisign_verify.py` (zig's public key is on ziglang.org/download)
2. `bash ffmpeg/verify_and_unpack.sh target/ffmpeg-min` (GPG checks in a throwaway keyring)
3. `bash ffmpeg/build_make.sh` (GNU make from source, if WSL has none)
4. `bash ffmpeg/build_ffmpeg.sh win64` (about 8 minutes); the program is in
   `target/ffmpeg-min/work/ffmpeg-win64/`. `DECODERS=aac,vp9` gives the 3.4 MB variant
   (5 of the 52 pairs under "What needs a decoder" then come out 8 to 35 bytes shorter
   than the full build's: H.264 / HEVC header fields only their decoders give; the
   packets are the same).

`ROOT=<folder>` (see `env.sh`) builds in another folder that already holds `build/`.
The builds of 2026-10-10: win64 4,231,680 bytes, macos-arm64 4,117,528 bytes.

## Proving it loses nothing

media.rs's join command was run with the minimal build and with a full one, and every
packet (size + MD5) compared with the full ffmpeg's framemd5. 2026-10-10, win64: 18 of 18 cases equal
(MP4 + M4A, VP9 in fragmented MP4 (8 and 10 bit) to MP4, WebM + WebM, odd pairs to MKV, TS
with H.264 / HEVC / AAC / AC-3 / E-AC-3 / MP3 to MP4, fragmented MP4 and DASH to MP4, ADTS to
M4A); every MP4 output byte-identical to the full build's; WebM / MKV differ only in
Matroska's random ids (identical with `-fflags +bitexact`). The build of 2026-10-08 passes
15 of these 18: the three VP9-to-MP4 cases were not among its checks. The macOS arm64 build
has not been run on a Mac yet.

## What needs a decoder

A copied stream's header is written from what the source file says, what the parsers find,
and, for the rest, what a decoder reads from the first frames. Where a muxer needs a value
that only a decoder gives and the decoder is not in, ffmpeg can write a wrong header and
still exit 0.

- VP9 into MP4 (YouTube's HLS pieces are VP9 in fragmented MP4): the `vpcC` box holds bit
  depth and chroma subsampling, taken from the pixel format. Without the VP9 decoder ffmpeg
  printed `Unsupported pixel format (-1)`, wrote an empty box, exited 0, and no ffmpeg could
  open the file (`Empty VP Codec Configuration box`). Tried first, smallest first: the VP9
  parser and `vp9_superframe` were already in; `vp9_metadata` (+24 KB) changes nothing, also
  when applied by hand; the decoder (+627 KB) makes the file byte-identical to the full
  build's, for profiles 0 to 3.
- 2026-10-10, 52 codec / container pairs run the way media.rs does, each result read back by
  the full ffmpeg and compared with the full build's own (packets, timestamps, stream
  fields): VP9 (profiles 0 to 3, HDR colours, full range), VP8, AV1 (8 and 10 bit), H.264,
  HEVC, AAC, Opus, Vorbis and FLAC into MP4 / M4A / WebM / MKV, from sample files and from
  test patterns. All equal. Two header values still differ from the full build's; the files
  read and decode the same:
  - Opus into WebM / MKV: the track's BitDepth stays what the source says (YouTube: 16); the
    full build writes 32, the sample size of its Opus decoder (which needs libswresample).
  - FLAC from fragmented MP4 into M4A: the sample size next to the codec name is 0 instead
    of 16; the FLAC header stored with it (`dfLa`) has the real value.
