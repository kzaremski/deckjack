#!/usr/bin/env bash
#
# Generates the iOS home-screen assets that a manifest cannot supply.
#
# Two things iOS does differently from every other installable-web target, and
# both are why these files have to exist:
#
#  1. It ignores the manifest's `icons` for the home screen and reads
#     `<link rel="apple-touch-icon">` instead. It also composites any
#     transparency onto black and applies its OWN rounded-corner mask, so the
#     source here is square and fully opaque. Feeding it the rounded 512 icon
#     produces a visibly double-rounded tile with dark corners.
#
#  2. Its launch screen comes from `apple-touch-startup-image`, matched by an
#     exact media query per device. A device with no match falls back to the
#     manifest's `background_color`, which is the same `#0A0D11` these images
#     are painted with — so an unlisted device degrades to a plain dark screen
#     rather than to a white flash.
#
# Idempotent: run it again after changing deckjack.svg.
#
#   ./scripts/emit-ios-assets.sh
#
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
icons="$here/public/icons"
splash="$here/public/splash"
ground="#0A0D11"

command -v magick >/dev/null || { echo "needs ImageMagick (magick)" >&2; exit 1; }
command -v rsvg-convert >/dev/null || { echo "needs rsvg-convert" >&2; exit 1; }

mkdir -p "$splash"

# --- the home-screen icon -------------------------------------------------
#
# Square corners and no alpha, per note 1 above. Built by stripping the `rx`
# from the source icon rather than keeping a second drawing in sync.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
sed 's/ rx="104"//' "$icons/deckjack.svg" > "$tmp/square.svg"

rsvg-convert -w 180 -h 180 "$tmp/square.svg" -o "$tmp/180.png"
magick "$tmp/180.png" -background "$ground" -alpha remove -alpha off "$icons/deckjack-180.png"
echo "icons/deckjack-180.png"

# --- launch screens -------------------------------------------------------
#
# logical-width logical-height dpr. Portrait and landscape are emitted for
# each. The logo is a sixth of the short edge, which keeps it the same visual
# size on a phone and on a 12.9" iPad.
rsvg-convert -w 512 -h 512 "$icons/deckjack.svg" -o "$tmp/logo.png"

devices=(
  "320 568 2"    # SE (1st gen), 5s
  "375 667 2"    # SE (2nd/3rd gen), 8, 7, 6s
  "414 736 3"    # 8 Plus, 7 Plus, 6s Plus
  "375 812 3"    # X, XS, 11 Pro, 12 mini, 13 mini
  "414 896 2"    # XR, 11
  "414 896 3"    # XS Max, 11 Pro Max
  "390 844 3"    # 12, 12 Pro, 13, 13 Pro, 14
  "428 926 3"    # 12 Pro Max, 13 Pro Max, 14 Plus
  "393 852 3"    # 14 Pro, 15, 15 Pro, 16
  "430 932 3"    # 14 Pro Max, 15 Plus, 15 Pro Max, 16 Plus
  "402 874 3"    # 16 Pro
  "440 956 3"    # 16 Pro Max
  "768 1024 2"   # iPad mini, iPad 9.7"
  "810 1080 2"   # iPad 10.2"
  "834 1112 2"   # iPad Air 10.5", Pro 10.5"
  "820 1180 2"   # iPad Air 10.9"
  "834 1194 2"   # iPad Pro 11"
  "1024 1366 2"  # iPad Pro 12.9"
)

made=0
for d in "${devices[@]}"; do
  read -r lw lh dpr <<<"$d"
  for orientation in portrait landscape; do
    if [ "$orientation" = portrait ]; then
      px=$((lw * dpr)); py=$((lh * dpr))
    else
      px=$((lh * dpr)); py=$((lw * dpr))
    fi
    short=$((px < py ? px : py))
    size=$((short / 6))
    out="$splash/splash-${px}x${py}.png"
    [ -f "$out" ] && continue
    magick -size "${px}x${py}" "xc:$ground" \
      \( "$tmp/logo.png" -resize "${size}x${size}" \) \
      -gravity center -composite \
      -strip "$out"
    made=$((made + 1))
  done
done

echo "splash/: $made new, $(ls -1 "$splash" | wc -l) total, $(du -sh "$splash" | cut -f1)"

# --- the link tags --------------------------------------------------------
#
# Written to a file rather than printed, because index.html has to carry them
# literally: iOS matches each one with a media query and there is no wildcard.
# Paste the contents into index.html's <head> when the device table changes.
links="$here/scripts/ios-splash-links.html"
: > "$links"
for d in "${devices[@]}"; do
  read -r lw lh dpr <<<"$d"
  for orientation in portrait landscape; do
    if [ "$orientation" = portrait ]; then
      px=$((lw * dpr)); py=$((lh * dpr))
    else
      px=$((lh * dpr)); py=$((lw * dpr))
    fi
    printf '<link rel="apple-touch-startup-image" href="/splash/splash-%sx%s.png" media="(device-width: %spx) and (device-height: %spx) and (-webkit-device-pixel-ratio: %s) and (orientation: %s)" />\n' \
      "$px" "$py" "$lw" "$lh" "$dpr" "$orientation" >> "$links"
  done
done
echo "scripts/ios-splash-links.html: $(wc -l < "$links") tags"

# --- splice them into index.html -----------------------------------------
#
# Between markers and idempotent, so re-running after a device is added
# replaces the block instead of appending a second copy.
index="$here/index.html"
python3 - "$index" "$links" <<'PY'
import re, sys
index_path, links_path = sys.argv[1], sys.argv[2]
tags = open(links_path).read().rstrip("\n")
body = "\n".join("    " + line for line in tags.splitlines())
src = open(index_path).read()
start = "<!-- ios-splash:start — generated by scripts/emit-ios-assets.sh, do not edit by hand -->"
end = "<!-- ios-splash:end -->"
pattern = re.compile(re.escape(start) + r".*?" + re.escape(end), re.S)
if not pattern.search(src):
    raise SystemExit("index.html is missing the ios-splash markers")
open(index_path, "w").write(pattern.sub(start + "\n" + body + "\n    " + end, src))
print(f"index.html: spliced {len(tags.splitlines())} startup-image tags")
PY
