#!/usr/bin/env bash
# Records Kuana playing an autopilot script (see src/autopilot.rs) and turns it into a GIF (macOS).
#
#   scripts/record.sh [options] [<script> [<folder> [<file>…]]]
#
# Defaults to scripts/readme.autopilot on this repository, saved as docs/screenshots/demo.gif.
# Launches target/release/kuana with a fresh config of its own (target/record-config: Welcome
# Setup done, the folder trusted, so nothing of your ~/.config/kuana shows; terminals run zsh with
# a plain prompt instead of your own), runs the script and
# records the window from its `mark start` to its `mark stop`. Kuana gets its key presses from the
# script, not from macOS, so nothing types into other apps and the window may be covered; only
# that window is captured, without the pointer.
#
# Options:
#   -o FILE   output GIF (default docs/screenshots/demo.gif)
#   -r FPS    frames per second (default 12)
#   -w PX     GIF width in pixels (default 1200)
#   -q N      gifski quality, 1–100 (default 80)
#   -k        keep the PNG frames (in target/record-frames)
#
# The terminal needs Screen Recording access. Needs gifski (`brew install gifski` or
# `cargo install gifski`).

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/docs/screenshots/demo.gif"
fps=12
width=1200
quality=80
keep=0

while getopts "o:r:w:q:kh" opt; do
    case $opt in
        o) out=$OPTARG ;;
        r) fps=$OPTARG ;;
        w) width=$OPTARG ;;
        q) quality=$OPTARG ;;
        k) keep=1 ;;
        *) sed -n '2,22s/^# \{0,1\}//p' "$0"; exit 0 ;;
    esac
done
shift $((OPTIND - 1))

script="${1:-$root/scripts/readme.autopilot}"
script="$(cd "$(dirname "$script")" && pwd)/$(basename "$script")"
(( $# > 0 )) && shift
(( $# > 0 )) || set -- "$root"
folder="$(cd "$1" && pwd)"

command -v gifski >/dev/null || { echo "Needs gifski: brew install gifski (or cargo install gifski)" >&2; exit 1; }
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
echo "$count frames ($(( count / fps )) s); making the GIF…"
mkdir -p "$(dirname "$out")"
gifski --quiet --fps "$fps" --width "$width" --quality "$quality" -o "$out" "$frames"/frame-*.png
[[ $keep == 1 ]] || rm -rf "$frames"
echo "Saved ${out#"$root"/} ($(du -h "$out" | cut -f1 | tr -d ' '))"
