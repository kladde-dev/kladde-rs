#!/usr/bin/env bash
# Shrink a copied criterion report in place: gzip every .svg to .svgz and
# rewrite the references in the .html files that point at them.
#
# SVG is XML, so it compresses ~8x; the reports are otherwise dominated by plot
# files. Note that browsers only decompress .svgz over file:// inconsistently
# (Firefox does, Chrome generally expects a Content-Encoding: gzip header), so
# if the plots show as broken images, serve the directory over HTTP or gunzip
# them back.
#
# Usage: test-results/svgz.sh <directory>
set -euo pipefail

dir="${1:?usage: svgz.sh <directory>}"

before=$(du -sh "$dir" | cut -f1)

find "$dir" -name '*.svg' -print0 | while IFS= read -r -d '' svg; do
    gzip -9 -c "$svg" > "${svg}z"
    rm "$svg"
done

# `plot.svg` -> `plot.svgz`, wherever it appears in an attribute or url().
find "$dir" -name '*.html' -print0 \
    | xargs -0 --no-run-if-empty sed -i -E 's/\.svg([")'"'"'])/.svgz\1/g'

after=$(du -sh "$dir" | cut -f1)
echo "$dir: $before -> $after ($(find "$dir" -name '*.svgz' | wc -l) plots compressed)"
