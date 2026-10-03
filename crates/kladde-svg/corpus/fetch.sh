#!/bin/sh
# Fetches the SVG corpus for svg-roundtrip and svg-bench into this directory.
#
# Nothing fetched is committed (see .gitignore), so these files stay under
# their own licenses, used locally:
#
#   drawings/tiger.svg               Ghostscript Tiger, AGPL-3.0-or-later
#                                    (Wikimedia Commons, from Ghostscript)
#   drawings/world-map.svg           BlankMap-World, public domain
#                                    (Wikimedia Commons, Canuckguy and others)
#   drawings/coat-of-arms-spain.svg  Escudo de España (mazonado), public domain
#                                    (Wikimedia Commons; an Inkscape drawing)
#   resvg/                           resvg's test suite at v0.48.1, MIT OR
#                                    Apache-2.0 (github.com/linebender/resvg)
#
# Every download is pinned by its SHA-256. Wikimedia Commons serves a file's
# latest revision, so a mismatch there means the file changed upstream: the
# script warns and keeps the new file, whose results will not compare with
# earlier runs.

set -eu
cd "$(dirname "$0")"
UA="kladde-svg-corpus/0.1 (https://github.com/kladde-dev/kladde-rs)"

fetch() {
    url=$1
    out=$2
    want=$3
    if [ ! -f "$out" ]; then
        echo "fetching $out" >&2
        curl -fsSL -A "$UA" -o "$out.part" "$url"
        mv "$out.part" "$out"
    fi
    got=$(sha256sum "$out" | cut -c1-64)
    if [ "$got" != "$want" ]; then
        echo "warning: $out has SHA-256 $got, not the pinned $want" >&2
    fi
}

mkdir -p drawings
fetch https://upload.wikimedia.org/wikipedia/commons/f/fd/Ghostscript_Tiger.svg \
    drawings/tiger.svg \
    5211e169283f43ab8ad7ea7998d917d5fbb3c568ac85c1a0217e86792822684d
fetch https://upload.wikimedia.org/wikipedia/commons/4/4d/BlankMap-World.svg \
    drawings/world-map.svg \
    88ae6cbcbe054c00455d16899724b348a102f49ff059bf7027c94e63b040e3e7
fetch "https://upload.wikimedia.org/wikipedia/commons/8/85/Escudo_de_Espa%C3%B1a_%28mazonado%29.svg" \
    drawings/coat-of-arms-spain.svg \
    5fb1742008948a0ab5865d3835778c140188c621a7c5642231fbedb7a47c22d3

fetch https://codeload.github.com/linebender/resvg/tar.gz/refs/tags/v0.48.1 \
    resvg-0.48.1.tar.gz \
    40dafea6b4b9d01e9d28b6d49f1e912daf3e9055676ad9179a5a2db6e7386945
if [ ! -d resvg ]; then
    mkdir -p resvg.part
    tar -xzf resvg-0.48.1.tar.gz -C resvg.part --strip-components=1 --wildcards \
        'resvg-0.48.1/crates/resvg/tests/*.svg' \
        'resvg-0.48.1/crates/usvg/tests/*.svg'
    mv resvg.part resvg
fi

echo "corpus: $(find drawings resvg -name '*.svg' | wc -l) SVG files" >&2
