#!/usr/bin/env bash
# Starts a Kuana build on a project and plays scripts/smoke.autopilot (see src/autopilot.rs):
# fails if Kuana can't start, a step fails, it panics or it doesn't finish in time. Saves a
# screenshot of the screen at each `mark shot` and Kuana's output to the output folder.
# Used by .github/workflows/smoke.yml on macOS, Windows (Git Bash) and Linux (under Xvfb). On a
# desktop the screenshots are of the whole screen; SMOKE_SHOTS=0 skips them.
#
#   scripts/smoke.sh <kuana binary> <project folder> <output folder>

set -euo pipefail

[[ $# == 3 ]] || { sed -n '2,8s/^# \{0,1\}//p' "$0"; exit 2; }
root="$(cd "$(dirname "$0")/.." && pwd)"
kuana=$1
project="$(cd "$2" && pwd)"
out=$3
mkdir -p "$out"
out="$(cd "$out" && pwd)"
limit=${SMOKE_TIMEOUT:-180}

# A config of its own: no Welcome Setup, the project trusted, no update checks.
config="$out/config"
rm -rf "$config"
mkdir -p "$config/kuana"
echo '{ "welcome_done": true }' >"$config/kuana/state.json"
echo '{ "auto_update": false }' >"$config/kuana/settings.json"
project_native=$project
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) windows=1; project_native=$(cygpath -w "$project") ;;
    *) windows=0 ;;
esac
# JSON-escaped by hand: Windows runners may have no python3.
escaped=${project_native//\\/\\\\}
printf '{ "folders": ["%s"] }\n' "${escaped//\"/\\\"}" >"$config/kuana/trust.json"
if [[ $windows == 1 ]]; then
    export APPDATA
    APPDATA=$(cygpath -w "$config")
else
    export XDG_CONFIG_HOME="$config"
fi

script="$root/scripts/smoke.autopilot"
[[ $windows == 1 ]] && script=$(cygpath -w "$script")
events="$out/kuana-output.log"
KUANA_AUTOPILOT="$script" RUST_BACKTRACE=1 "$kuana" "$project_native" </dev/null >"$events" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT

screenshot() {
    [[ ${SMOKE_SHOTS:-1} == 0 ]] && return
    local file="$out/$1.png"
    case "$(uname -s)" in
        Darwin) screencapture -x "$file" ;;
        Linux) import -window root "$file" ;;
        *) powershell -NoProfile -Command "
            Add-Type -AssemblyName System.Windows.Forms, System.Drawing
            \$b = [System.Windows.Forms.SystemInformation]::VirtualScreen
            \$i = New-Object System.Drawing.Bitmap \$b.Width, \$b.Height
            [System.Drawing.Graphics]::FromImage(\$i).CopyFromScreen(\$b.Left, \$b.Top, 0, 0, \$i.Size)
            \$i.Save('$(cygpath -w "$file")')" ;;
    esac || echo "Couldn't take $1.png"
}

shots=0
start=$SECONDS
while kill -0 "$pid" 2>/dev/null; do
    if (( SECONDS - start > limit )); then
        screenshot timeout
        echo "::error::Kuana didn't finish the smoke test in ${limit} s"
        cat "$events"
        exit 1
    fi
    marks=$(grep -c "kuana-autopilot: mark shot" "$events" || true)
    if (( marks > shots )); then
        shots=$((shots + 1))
        screenshot "shot-$shots"
    fi
    sleep 0.2
done
status=0
wait "$pid" || status=$?
cat "$events"

failed=0
if [[ $status != 0 ]]; then
    echo "::error::Kuana exited with $status"
    failed=1
fi
grep -q "kuana-autopilot: mark done" "$events" || { echo "::error::The smoke script didn't reach its end"; failed=1; }
for log in "$config"/kuana/logs/panic-*.log; do
    [[ -e $log ]] || continue
    echo "::error::Kuana panicked: $(basename "$log")"
    cat "$log"
    failed=1
done
(( failed == 0 )) && echo "Smoke test passed ($((SECONDS - start)) s, $shots screenshots)"
exit $failed
