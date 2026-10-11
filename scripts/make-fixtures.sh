#!/bin/sh
# Regenerates tests/fixtures/audio/a.{flac,mp3,m4a,ogg,opus}. This is a
# maintainer tool for Unix. It needs ffmpeg with libmp3lame, libvorbis and
# libopus on PATH. The test suite reads the committed files and does not run
# this script.
#
# Each file holds a 0.5 s mono 440 Hz sine. The sample rate is 22050 Hz for
# every file except a.opus: libopus accepts 8000, 12000, 16000, 24000 and
# 48000 Hz only, so a.opus is encoded at 24000 Hz.
#
# The files carry no tags. The bitexact flags keep the ffmpeg version out of
# the files, and the empty encoder value removes the encoder comment that
# ffmpeg otherwise writes into the two Ogg files. A rerun with the same ffmpeg
# build writes the same bytes.
set -eu

out="$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures/audio"
mkdir -p "$out"

# encode <sample rate> <file name> <codec options...>
encode() {
    rate="$1"
    name="$2"
    shift 2
    ffmpeg -hide_banner -loglevel error -y \
        -f lavfi -i "sine=frequency=440:duration=0.5" -ac 1 -ar "$rate" \
        "$@" -fflags +bitexact -flags:a +bitexact -metadata:s:a:0 encoder= \
        "$out/$name"
}

encode 22050 a.flac -c:a flac
encode 22050 a.mp3 -c:a libmp3lame -b:a 32k
encode 22050 a.m4a -c:a aac -b:a 32k
encode 22050 a.ogg -c:a libvorbis -q:a 0
encode 24000 a.opus -c:a libopus -b:a 24k

wc -c "$out"/a.flac "$out"/a.mp3 "$out"/a.m4a "$out"/a.ogg "$out"/a.opus
