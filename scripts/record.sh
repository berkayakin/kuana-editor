#!/usr/bin/env bash
# Records Kuana playing an autopilot script (see src/autopilot.rs) as an animated WebP (macOS).
#
#   scripts/record.sh [options] [<script> [<folder> [<file>…]]]
#
# Defaults to scripts/readme.autopilot on this repository, saved as docs/screenshots/demo.webp.
# Launches target/release/kuana with a fresh config of its own (target/record-config: Welcome
# Setup done, the folder trusted, so nothing of your ~/.config/kuana shows; terminals run zsh with
# a plain prompt instead of your own), runs the script and
# records the window from its `mark start` to its `mark stop`. Kuana gets its key presses from the
# script, not from macOS, so nothing types into other apps and the window may be covered; only
# that window is captured, without the pointer, then framed like a screenshot (rounded corners, a
# margin and a soft shadow on transparency, which is why it's WebP: GIF has no soft transparency).
#
# Options:
#   -o FILE   output (default docs/screenshots/demo.webp)
#   -r FPS    frames per second at most (default 14)
#   -w PX     the window's width in the output, in pixels (default 1280)
#   -q N      WebP quality, 1–100 (default 80)
#   -k        keep the PNG frames (in target/record-frames)
#
# The terminal needs Screen Recording access. Needs img2webp (`brew install webp`).

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/docs/screenshots/demo.webp"
fps=14
width=1280
quality=80
keep=0

while getopts "o:r:w:q:kh" opt; do
    case $opt in
        o) out=$OPTARG ;;
        r) fps=$OPTARG ;;
        w) width=$OPTARG ;;
        q) quality=$OPTARG ;;
        k) keep=1 ;;
        *) sed -n '2,24s/^# \{0,1\}//p' "$0"; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

script="${1:-$root/scripts/readme.autopilot}"
script="$(cd "$(dirname "$script")" && pwd)/$(basename "$script")"
(( $# > 0 )) && shift
(( $# > 0 )) || set -- "$root"
folder="$(cd "$1" && pwd)"

command -v img2webp >/dev/null || { echo "Needs img2webp: brew install webp" >&2; exit 1; }
kuana="$root/target/release/kuana"
[[ -x $kuana ]] || { echo "No $kuana; run cargo build --release first" >&2; exit 1; }

helper="$root/target/screenshot-helper"
if [[ ! -x $helper || $root/scripts/screenshot.swift -nt $helper ]]; then
    echo "Building the screenshot helper…"
    mkdir -p "$root/target"
    swiftc -O "$root/scripts/screenshot.swift" -o "$helper"
fi

config="$root/target/record-config"
rm -rf "$config"
mkdir -p "$config/kuana"
echo '{ "welcome_done": true }' >"$config/kuana/state.json"
python3 -c 'import json, sys; print(json.dumps({"folders": [sys.argv[1]]}))' "$folder" >"$config/kuana/trust.json"
# Terminals get zsh with a plain prompt: no user or host name in the recording.
mkdir -p "$config/zsh"
echo "PROMPT='%F{green}%1~%f %# '" >"$config/zsh/.zshrc"

frames="$root/target/record-frames"
rm -rf "$frames"
mkdir -p "$frames"
events="$root/target/record-events.log"
: >"$events"

SHELL=/bin/zsh ZDOTDIR="$config/zsh" XDG_CONFIG_HOME="$config" KUANA_AUTOPILOT="$script" "$kuana" "$@" </dev/null >"$events" 2>&1 &
pid=$!
recorder=""
# Only what this script started, by PID: other Kuana windows may be open.
cleanup() {
    [[ -n $recorder ]] && kill "$recorder" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
    exec 3>&- 2>/dev/null || true
}
trap cleanup EXIT

# Waits until the events log has `line` (or Kuana ends).
wait_for() {
    until grep -q "$1" "$events"; do
        kill -0 "$pid" 2>/dev/null || { echo "Kuana ended before “$1”:" >&2; cat "$events" >&2; exit 1; }
        sleep 0.05
    done
}

wait_for "kuana-autopilot: mark start"
window=$("$helper" window "$pid") || { echo "Kuana's window didn't appear" >&2; exit 1; }
# The recorder stops at a line on its stdin: a pipe kept open on fd 3.
pipe="$root/target/record-pipe"
rm -f "$pipe"
mkfifo "$pipe"
"$helper" record "$pid" "$window" "$frames" "$fps" <"$pipe" &
recorder=$!
exec 3>"$pipe"
rm -f "$pipe"
echo "Recording…"

wait_for "kuana-autopilot: mark stop"
echo stop >&3
exec 3>&-
wait "$recorder" || { echo "The recorder failed" >&2; exit 1; }
recorder=""
wait "$pid" || true

count=$(find "$frames" -name 'frame-*.png' | wc -l | tr -d ' ')
(( count > 0 )) || { echo "No frames were recorded" >&2; exit 1; }
echo "$count frames ($(( $(tail -1 "$frames/times.txt") / 1000 )) s); framing them…"
framed="$frames/framed"
mkdir -p "$framed"
"$helper" compose "$frames" "$framed" "$width"
echo "Making the WebP…"
# Each frame lasts until the next was taken (times.txt: milliseconds from the start).
args=()
stamps=()
while read -r t || [[ -n $t ]]; do stamps+=("$t"); done <"$frames/times.txt"
for (( i = 0; i < ${#stamps[@]}; i++ )); do
    if (( i + 1 < ${#stamps[@]} )); then
        duration=$(( stamps[i + 1] - stamps[i] ))
    else
        # A pause on the last frame before it starts over.
        duration=2000
    fi
    args+=(-d "$duration" "$framed/$(printf 'frame-%05d.png' $((i + 1)))")
done
mkdir -p "$(dirname "$out")"
img2webp -loop 0 -lossy -q "$quality" -m 6 "${args[@]}" -o "$out" >/dev/null
[[ $keep == 1 ]] || rm -rf "$frames"
echo "Saved ${out#"$root"/} ($(du -h "$out" | cut -f1 | tr -d ' '))"
