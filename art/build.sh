#!/bin/bash
# builds factorio-format icons from the generated glyph svg and item png
set -euo pipefail
cd "$(dirname "$0")"
out=${1:?graphics dir}
mkdir -p "$out"

# vanilla toggle shortcuts (roboport, exoskeleton, alt mode) are a #1D1D1D glyph on alpha;
# the alpha is the darkness of the black-on-white render
magick -background white -density 300 glyph-a.svg -resize 1024x1024 -alpha remove -alpha off glyph-render.png
magick glyph-render.png -colorspace Gray -negate -trim +repage -alpha copy -fill '#1D1D1D' -colorize 100 glyph-master.png

# one mip level: glyph fitted into <fit> px, centered on a <size> px canvas
level() { # <size> <fit> <outfile>
  magick glyph-master.png -filter Lanczos -resize "$2x$2" -background none -gravity center -extent "$1x$1" "$3"
}
# vanilla shortcut glyphs fill 40 of 56 px and carry one half-size mip to the right
level 56 40 l56.png; level 28 20 l28.png; level 24 18 l24.png; level 12 9 l12.png
magick -background none l56.png l28.png +append -gravity north -extent 84x56 -colorspace Gray -define png:color-type=4 "$out/ptt-radio-x56.png"
magick -background none l24.png l12.png +append -gravity north -extent 36x24 -colorspace Gray -define png:color-type=4 "$out/ptt-radio-x24.png"

# mod portal / mod manager thumbnail
magick item.png -filter Lanczos -resize 128x128 -background none -gravity center -extent 144x144 "$out/../thumbnail.png"
rm -f l56.png l28.png l24.png l12.png
file "$out"/*.png "$out/../thumbnail.png"
